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
pub mod metrics;
pub mod theme;
pub mod views;
pub mod widgets;
pub mod workspace_runtime;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

use chrono::{DateTime, Timelike, Utc};
use usagi_core::domain::AppInfo;
use usagi_core::domain::agent::{
    AgentInventory, AgentProfileId, AgentResumeRelation, AgentResumeTarget,
    AgentRuntimeInventoryState, ProviderResumeProjection,
};
use usagi_core::domain::id::{
    AgentContinuationRef, OperationId, SessionId, TerminalRef, UserDecisionId, WorkspaceId,
};
use usagi_core::domain::recent::Recent;
use usagi_core::domain::session_lifecycle::{SessionLifecycle, SessionLifecycleProjection};
use usagi_core::domain::terminal_launch::{TerminalInventoryEntry, TerminalKind};
use usagi_core::domain::user_decision::UserDecisionAnswer;
use usagi_core::domain::workspace::Workspace;
use usagi_core::usecase::client::DaemonMetrics;
use usagi_core::usecase::env::EnvScope;

use crate::presentation::live_terminal::{LiveTerminalControls, PointerRelease};
use crate::presentation::metrics::{MetricsBackend, MetricsProjection};
use crate::presentation::theme::{Color, Style};
use crate::presentation::views::config::{self, AvailableAgentModels, Config};
use crate::presentation::views::create_session_error_modal;
use crate::presentation::views::director_drawer::{
    self, DirectorConversation, DirectorDrawerProjection, DirectorNewProjection,
    DirectorOrganizationRow,
};
use crate::presentation::views::new::{self, Field, New};
use crate::presentation::views::open::{self, Open};
use crate::presentation::views::quit_modal;
use crate::presentation::views::scratchpad_modal;
use crate::presentation::views::splash;
use crate::presentation::views::welcome::{self, MenuAction, Welcome};
use crate::presentation::views::workspace::{
    self, GitDiff, HomeHeaderAction, HomeProjection, ProjectedSession, TerminalViewProjection,
    Workspace as WorkspaceView, garden_click_at, garden_fits, home_header_action_at, render_home,
    render_home_at, right_pane_tab_at, terminal_point_at,
};
use crate::presentation::widgets::modal::{self, ConfirmationView};
use crate::presentation::workspace_runtime::{PaneRestoreTarget, WorkspaceRuntime};
use crate::usecase::application::agent_tab_intent::{
    AgentTabIntent, AgentTabIntentError, AgentTabIntentMutation, AgentTabIntentPort,
    AgentTabIntentPortCommit, AgentTabProjection,
};
use crate::usecase::application::controller::{
    AppEvent, AppKey, AppState, BackendEvent, DirectorNew, Effect, EnvironmentEntry, ExitChoice,
    Feedback, GardenClick, NewRequest, Notice, OperationResult, Overlay, PendingToken, RoleChoice,
    SessionRoleCatalog, SessionRoleProjection, Target,
};
#[cfg(test)]
use crate::usecase::application::controller::{SafeError, SafeMessage};
use crate::usecase::application::daemon_backend::{
    AgentPort as BackendAgentPort, Completions, CreateSessionRequest, DaemonBackend,
    DecisionPort as BackendDecisionPort, Flow as BackendFlow, LaunchAgentRequest,
    OpenTerminalRequest, OverlayPort as BackendOverlayPort, RemoveSessionRequest,
    ReopenAgentRequest, ResumeAgentRequest, SessionCommandPort as BackendSessionCommandPort,
    TargetStorePort as BackendTargetStorePort, WorkspaceCommandPort as BackendWorkspaceCommandPort,
};
use crate::usecase::application::interrupted_tab::{InterruptedTab, ResumeCommand};
use crate::usecase::application::pane::{PaneKind, PaneSelection, PaneTab, TabSelection};
use crate::usecase::application::pane_runtime::Geometry;
use crate::usecase::application::pr::{BrowserOpener, PrSnapshotPort};
use crate::usecase::application::terminal_screen::TerminalBuffer;
use crate::usecase::application::terminal_screen::TerminalInputModes;
use crate::usecase::application::terminal_selection::TerminalSelection;
use crate::usecase::application::terminal_session::{
    SessionState, TerminalAttach, TerminalChunk, TerminalError, TerminalInputOutcome,
    TerminalInputResolution, TerminalSession, TerminalStreamPort, TerminalSubscription,
};
use crate::usecase::application::{Key, ScreenRunner, Terminal, open_refusal_notice};
use crate::usecase::overview::SessionCommand;
use crate::usecase::terminal_input::{
    LiveTerminalAction, PointerEvent, PointerKind, WHEEL_LINES, encode_mouse_wheel,
    encode_wheel_arrows,
};
use usagi_core::usecase::settings::SettingsPort;

pub use crate::usecase::application::{
    WorkspaceCreateCompletion, WorkspaceCreateEffect, WorkspaceCreateToken, WorkspaceLoader,
    WorkspaceSnapshot,
};

/// Daemon-authoritative Agent launch boundary for the workspace runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPaneAdmission {
    pub terminal: TerminalRef,
    pub continuation: Option<AgentContinuationRef>,
}

/// Daemon client vocabulary for one workspace's Agent / terminal boundary.
///
/// The controller binds **separate instances** of this port to separate roles:
/// the resident terminal stream (attach / poll / input / resize / detach) that
/// stays with the live panes, the dedicated restore client, and — behind
/// [`SerializedPaneLaunchPort`] — the pane launch client. A role never takes
/// another role's instance, so a slow or hung request in one cannot stop the
/// others.
pub trait AgentCommandPort: Send {
    /// Launch one daemon-owned Agent under the caller's durable operation.
    ///
    /// `operation` is the identity the controller already issued for the pending
    /// pane. It reaches the daemon unchanged and the implementation correlates the
    /// admission and the final answer back to it (#522); an implementation must
    /// never mint an operation of its own, because the side effect of a second
    /// identity could then be promoted as this pending pane's completion.
    ///
    /// # Errors
    ///
    /// Returns a presentation-safe daemon launch failure.
    fn launch(
        &mut self,
        operation: OperationId,
        workspace: WorkspaceId,
        session: Option<SessionId>,
        profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String>;

    /// Explicitly resumes retained provider-native metadata in a new daemon
    /// runtime. Implementations must not attach to the old PTY.
    ///
    /// # Errors
    ///
    /// Returns safe feedback when the daemon rejects the resume or does not
    /// return a fully fenced terminal reference.
    fn resume(
        &mut self,
        _workspace: WorkspaceId,
        _session: SessionId,
        _operation_id: OperationId,
    ) -> Result<AgentPaneAdmission, String> {
        Err("Agent resume is unavailable.".to_owned())
    }

    /// Returns the daemon's safe exact-target inventory for root and managed
    /// Agent histories in one workspace.
    ///
    /// # Errors
    ///
    /// Returns safe feedback when the daemon rejects the workspace inventory
    /// request or returns an invalid projection.
    fn resume_inventory(&mut self, _workspace: WorkspaceId) -> Result<AgentInventory, String> {
        Err("Agent resume inventory is unavailable.".to_owned())
    }

    /// Resumes only the exact daemon-issued target selected by the caller.
    ///
    /// The answer must carry the daemon's source-to-replacement relation: the
    /// TUI accepts a replacement only when that relation, the lineage, and a new
    /// fully fenced terminal all agree (#510).
    ///
    /// # Errors
    ///
    /// Returns safe feedback when the daemon rejects the exact target or does
    /// not return a fully fenced replacement terminal.
    fn resume_exact(
        &mut self,
        _target: AgentResumeTarget,
        _operation_id: OperationId,
    ) -> Result<ExactAgentResume, String> {
        Err("Exact Agent resume is unavailable.".to_owned())
    }

    /// Open a daemon-owned login shell for a scope. `session` is absent for the
    /// workspace root, whose checkout the daemon resolves to the trusted
    /// repository root.
    ///
    /// The default keeps embedders that only expose Agent launch safe: the
    /// Terminal action becomes an inline failure instead of spawning anything
    /// locally.
    ///
    /// # Errors
    ///
    /// Returns a presentation-safe launch failure.
    fn launch_terminal(
        &mut self,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _geometry: Geometry,
        _arguments: &str,
        _operation: OperationId,
    ) -> Result<TerminalRef, String> {
        Err("terminal launch is unavailable".to_owned())
    }

    /// Ask a daemon-owned terminal to take the visible pane viewport, and
    /// answer with the geometry it holds.
    ///
    /// One daemon terminal may be open in several windows, and its single PTY
    /// takes the smallest viewport among them, so the answer is not always the
    /// request ([`TerminalStreamPort::resize`]).
    ///
    /// # Errors
    ///
    /// Returns a safe daemon communication or terminal-ownership failure.
    fn resize_terminal(
        &mut self,
        _terminal: &TerminalRef,
        geometry: Geometry,
    ) -> Result<Geometry, TerminalError> {
        Ok(geometry)
    }

    /// Attach to a daemon-owned terminal, taking its retained replay and cursor.
    ///
    /// The default keeps embedders without a terminal stream safe: attach fails
    /// and the pane shows only the tab, never a locally spawned process.
    ///
    /// # Errors
    ///
    /// Returns a safe daemon communication or terminal-ownership failure.
    fn attach_terminal(
        &mut self,
        _terminal: &TerminalRef,
        _geometry: Geometry,
    ) -> Result<TerminalAttach, TerminalError> {
        Err(TerminalError::Unavailable)
    }

    /// Fetch the daemon terminal output produced after `after_offset`.
    ///
    /// # Errors
    ///
    /// Returns a safe daemon communication or terminal-ownership failure.
    fn poll_terminal(
        &mut self,
        _terminal: &TerminalRef,
        _after_offset: u64,
    ) -> Result<Vec<TerminalChunk>, TerminalError> {
        Err(TerminalError::Unavailable)
    }

    /// The epoch of the shared terminal transport this port currently holds, or
    /// `None` when it multiplexes nothing and therefore invalidates nothing.
    ///
    /// The production adapter carries every pane's attach / input / detach on
    /// one connection, so replacing it invalidates all subscriptions taken
    /// before. Reporting the epoch is what lets each [`TerminalSession`]
    /// re-attach on its own before it spends a keystroke.
    fn terminal_connection_epoch(&self) -> Option<u64> {
        None
    }

    /// Send input bytes to a daemon terminal, fenced by subscription/sequence.
    ///
    /// # Errors
    ///
    /// Returns a safe daemon communication or terminal-ownership failure.
    fn input_terminal(
        &mut self,
        _terminal: &TerminalRef,
        _subscription: TerminalSubscription,
        _input_seq: u64,
        _operation: OperationId,
        _bytes: &[u8],
    ) -> Result<TerminalInputOutcome, TerminalError> {
        Err(TerminalError::Unavailable)
    }

    /// Read the recorded final of one durable terminal input operation (#519).
    ///
    /// The default answers unknown, which is the fail-closed behaviour for an
    /// embedder without a durable daemon ledger: the pane keeps its uncertainty
    /// latched instead of writing the bytes again.
    ///
    /// # Errors
    ///
    /// Returns a safe daemon communication or terminal-ownership failure.
    fn terminal_input_outcome(
        &mut self,
        _terminal: &TerminalRef,
        _operation: OperationId,
        _input_len: usize,
    ) -> Result<TerminalInputResolution, TerminalError> {
        Ok(TerminalInputResolution::Unknown)
    }

    /// Release a daemon terminal subscription; it must not stop the process.
    /// A subscription from a replaced epoch is released locally, without
    /// touching the current transport or the attachments its peers hold.
    fn detach_terminal(&mut self, _terminal: &TerminalRef, _subscription: TerminalSubscription) {}

    /// Declare the **detached background** terminals whose exit the client still
    /// has to notice.
    ///
    /// Only the selected foreground terminal is attached, so a background tab
    /// has no stream that could report its process exiting. The production
    /// adapter observes these refs through a bounded per-scope terminal
    /// inventory on its own thread — never by attaching or resuming one of them
    /// — and reports each exit once through
    /// [`take_exited_background_terminals`](Self::take_exited_background_terminals).
    /// Their **final output bytes** are not fetched here: they are read when the
    /// tab is brought to the foreground, or through the explicit read-only
    /// reopen of the retained tombstone.
    ///
    /// The default keeps embedders without a daemon safe: nothing is observed,
    /// so no tab is ever closed behind the user's back.
    fn watch_background_terminals(&mut self, _terminals: &[TerminalRef]) {}

    /// Drain the background terminals observed as no longer live since the last
    /// call, at most `limit` per frame so one frame's work stays bounded.
    fn take_exited_background_terminals(&mut self, _limit: usize) -> Vec<TerminalRef> {
        Vec::new()
    }

    /// List the daemon-owned runtimes in scope for this workspace so a freshly
    /// opened controller can re-project the terminals and Agents that are still
    /// live into pane tabs. The production adapter resolves the workspace root
    /// and every available session scope and unions the daemon inventory.
    ///
    /// The default keeps embedders without a daemon safe: no runtime is
    /// discovered, so opening a workspace simply starts with no restored panes.
    ///
    /// # Errors
    ///
    /// Returns a safe daemon communication failure; the caller then restores
    /// nothing rather than spawning anything locally.
    fn list_terminals(&mut self) -> Result<Vec<TerminalInventoryEntry>, TerminalError> {
        Ok(Vec::new())
    }
}

/// Daemon boundary for one pane launch request.
///
/// Deliberately separate from the resident [`AgentCommandPort`] that streams the
/// live panes: a launch worker *borrows* this port through a shared `Arc` and
/// never owns the stream port, so a slow, hung, or panicking launch can no
/// longer take an existing pane's subscription, poll, input, resize, or detach
/// with it. `&self` is exactly the shape session command admission already uses,
/// so a lost or late completion cannot strand the capability either.
///
/// One instance answers every launch of the workspace — Agent and generic
/// terminal, workspace root and session, foreground and background — so all of
/// them obey the same ownership rule. Implementations whose client is a single
/// request sequence serialize internally ([`SerializedPaneLaunchPort`]).
pub trait PaneLaunchCommandPort: Send + Sync {
    /// # Errors
    ///
    /// Returns a presentation-safe daemon launch failure.
    /// `operation` is the pending pane's durable launch identity; it reaches the
    /// daemon unchanged so the admission and the final can be correlated back to
    /// exactly this pane instead of to an adapter-minted operation (#522).
    fn launch(
        &self,
        operation: OperationId,
        workspace: WorkspaceId,
        session: Option<SessionId>,
        profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String>;

    /// # Errors
    ///
    /// Returns safe feedback when the daemon rejects the resume or does not
    /// return a fully fenced terminal reference.
    fn resume(
        &self,
        workspace: WorkspaceId,
        session: SessionId,
        operation: OperationId,
    ) -> Result<AgentPaneAdmission, String>;

    /// # Errors
    ///
    /// Returns safe feedback when the daemon rejects the exact target or does
    /// not return a fully fenced replacement terminal.
    fn resume_exact(
        &self,
        target: AgentResumeTarget,
        operation: OperationId,
    ) -> Result<ExactAgentResume, String>;

    /// # Errors
    ///
    /// Returns a presentation-safe launch failure.
    /// `operation` is the controller's durable launch identity; it reaches the
    /// daemon unchanged so a repeated delivery replays instead of spawning.
    fn launch_terminal(
        &self,
        workspace: WorkspaceId,
        session: Option<SessionId>,
        geometry: Geometry,
        arguments: &str,
        operation: OperationId,
    ) -> Result<TerminalRef, String>;
}

/// Adapts one **dedicated** [`AgentCommandPort`] client to the shared launch
/// contract by serializing the requests admitted to it.
///
/// The wrapped client must be a different instance from the resident stream
/// port; that separation is what keeps a hung launch away from pane IO. A worker
/// that panics inside the client poisons this mutex and the guard is recovered:
/// every launch method opens and finishes its own daemon request, so the next
/// launch inherits no partial state, while the panicking pane has already
/// completed as a safe failure.
pub struct SerializedPaneLaunchPort(std::sync::Mutex<Box<dyn AgentCommandPort>>);

impl SerializedPaneLaunchPort {
    /// Bind `port` as the launch client of one workspace.
    #[must_use]
    pub fn new(port: Box<dyn AgentCommandPort>) -> Self {
        Self(std::sync::Mutex::new(port))
    }

    fn client(&self) -> std::sync::MutexGuard<'_, Box<dyn AgentCommandPort>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl PaneLaunchCommandPort for SerializedPaneLaunchPort {
    fn launch(
        &self,
        operation: OperationId,
        workspace: WorkspaceId,
        session: Option<SessionId>,
        profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        self.client().launch(operation, workspace, session, profile)
    }

    fn resume(
        &self,
        workspace: WorkspaceId,
        session: SessionId,
        operation: OperationId,
    ) -> Result<AgentPaneAdmission, String> {
        self.client().resume(workspace, session, operation)
    }

    fn resume_exact(
        &self,
        target: AgentResumeTarget,
        operation: OperationId,
    ) -> Result<ExactAgentResume, String> {
        self.client().resume_exact(target, operation)
    }

    fn launch_terminal(
        &self,
        workspace: WorkspaceId,
        session: Option<SessionId>,
        geometry: Geometry,
        arguments: &str,
        operation: OperationId,
    ) -> Result<TerminalRef, String> {
        self.client()
            .launch_terminal(workspace, session, geometry, arguments, operation)
    }
}

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

/// Platform-native terminal launch boundary.
///
/// This is deliberately independent from [`AgentCommandPort`]: `terminal new`
/// must remain available without any daemon client.
pub trait ExternalTerminalPort: Send {
    /// Open a native terminal rooted at `directory`.
    ///
    /// # Errors
    ///
    /// Returns a presentation-safe platform launch failure.
    fn open(&mut self, directory: &Path) -> Result<(), String>;
}

/// Daemon-authoritative durable decision boundary for the workspace runtime.
///
/// The controller keeps the list and editor locally, while this port is the
/// only route that can refresh or resolve daemon-owned decisions.  Responses
/// are projected back through [`BackendEvent`], preserving the reducer's
/// one-way event flow and making the production adapter replaceable by a fake.
pub trait DecisionCommandPort: Send {
    /// Fetch the authoritative pending snapshot for one workspace.
    fn refresh(&mut self, workspace: WorkspaceId) -> BackendEvent;
    /// Submit one already validated answer. Rows remain visible until the
    /// returned confirmation event reaches the reducer.
    fn resolve(
        &mut self,
        workspace: WorkspaceId,
        decision_id: UserDecisionId,
        answer: UserDecisionAnswer,
    ) -> BackendEvent;
}

/// Durable per-target environment boundary for the workspace runtime.
///
/// The controller keeps the editor's draft locally; this port is the only route
/// that reads and writes the persisted environment bindings of one
/// [`EnvScope`] — this workspace's own, or the global ones every workspace
/// inherits. Both operations project their result back through
/// [`BackendEvent`] (`EnvironmentLoaded` / `EnvironmentError`), preserving the
/// reducer's one-way event flow and keeping the editor's values through a save
/// failure. Mapping a scope to its settings file is the implementation's concern.
pub trait EnvironmentStorePort: Send {
    /// Read `scope`'s bindings, plus the global ones it inherits.
    fn load(&mut self, scope: EnvScope) -> BackendEvent;
    /// Persist the complete set of `entries` for `scope`, replacing what was
    /// stored. On success the saved set refluxes as `EnvironmentLoaded`.
    fn save(&mut self, scope: EnvScope, entries: Vec<EnvironmentEntry>) -> BackendEvent;
}

/// Best-effort desktop-notification boundary for newly observed user decisions.
///
/// The TUI never depends on an OS command: unsupported platforms and delivery
/// failures are handled by the composition adapter, while the notice centre
/// remains usable.
pub trait DesktopNotificationPort {
    fn notify(&mut self, title: &str, body: &str);
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
struct AgentStreamPort<'a>(&'a mut dyn AgentCommandPort);

impl TerminalStreamPort for AgentStreamPort<'_> {
    fn connection_epoch(&self) -> Option<u64> {
        self.0.terminal_connection_epoch()
    }

    fn resize(
        &mut self,
        terminal: &TerminalRef,
        geometry: Geometry,
    ) -> Result<Geometry, TerminalError> {
        self.0.resize_terminal(terminal, geometry)
    }

    fn attach(
        &mut self,
        terminal: &TerminalRef,
        geometry: Geometry,
    ) -> Result<TerminalAttach, TerminalError> {
        self.0.attach_terminal(terminal, geometry)
    }
    fn poll(
        &mut self,
        terminal: &TerminalRef,
        after_offset: u64,
    ) -> Result<Vec<TerminalChunk>, TerminalError> {
        self.0.poll_terminal(terminal, after_offset)
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
            .input_terminal(terminal, subscription, input_seq, operation, bytes)
    }
    fn input_outcome(
        &mut self,
        terminal: &TerminalRef,
        operation: OperationId,
        input_len: usize,
    ) -> Result<TerminalInputResolution, TerminalError> {
        self.0
            .terminal_input_outcome(terminal, operation, input_len)
    }
    fn detach(&mut self, terminal: &TerminalRef, subscription: TerminalSubscription) {
        self.0.detach_terminal(terminal, subscription);
    }
}

/// Maps a management [`Key`] to the bytes a focused live terminal should
/// receive. Reserved prefix actions ([`Key::Live`]) do not reach the shell;
/// all other keys, including global controls, do while Closeup owns the pane.
fn key_to_terminal_bytes(key: Key) -> Option<Vec<u8>> {
    let bytes = match key {
        Key::Passthrough(bytes) => return (!bytes.is_empty()).then(|| bytes.clone()),
        Key::Management { passthrough, .. } => {
            return (!passthrough.is_empty()).then_some(passthrough);
        }
        // Forward a paste as one bracketed-paste block so an agent that requested
        // the mode inserts the multi-line text instead of submitting on every
        // embedded newline (the fix for pasting clipboard into the agent).
        Key::Paste(text) => {
            return (!text.is_empty())
                .then(|| crate::usecase::terminal_input::encode_bracketed_paste(&text));
        }
        Key::Char(ch) => ch.to_string().into_bytes(),
        Key::Enter => b"\r".to_vec(),
        Key::Backspace => b"\x7f".to_vec(),
        Key::Tab => b"\t".to_vec(),
        Key::Escape => b"\x1b".to_vec(),
        Key::Up => b"\x1b[A".to_vec(),
        Key::Down => b"\x1b[B".to_vec(),
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
    ui: &mut WorkspaceUi,
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
    let Some((terminal, bytes)) = runtime
        .wants_live_input()
        .then(|| runtime.focused_terminal())
        .flatten()
        .zip(key_to_terminal_bytes(key.clone()))
    else {
        return false;
    };
    // The stream port is resident, so a launch in flight never drops a
    // keystroke; a genuine stream failure is surfaced instead of swallowed.
    if let Err(message) = ui.send_terminal_bytes(&terminal, &bytes) {
        controls.set_feedback(message);
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
        && (runtime.state().director_launching().is_some()
            || !matches!(runtime.state().director_new(), DirectorNew::Idle))
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
        return match key {
            Key::Resize | Key::Other => None,
            _ => Some(runtime.handle_key(key.clone())),
        };
    }
    // The conversation is not exclusive because its selected root Agent owns
    // ordinary terminal input. Its drawer-local open/close operations still
    // precede PTY forwarding and the Home reducer.
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

/// Retarget the two `Ctrl-O` follow-ups whose meaning differs in Director mode.
///
/// In the drawer, New is the operation a control chord should reach — `Ctrl-O`
/// `Ctrl-N` opens the CLI picker — and conversation cycling takes the plain
/// follow-up `Ctrl-O` `n` in its place. Outside the drawer both chords keep
/// their managed-pane meaning (`Ctrl-O Ctrl-N` cycles tabs, `Ctrl-O n` opens the
/// drawer's New picker), so this swap is scoped to an open drawer and applied
/// once, before any consumer of the key observes it.
fn retarget_director_chords(runtime: &WorkspaceRuntime, key: Key) -> Key {
    if !runtime.state().director_drawer_open() {
        return key;
    }
    match key {
        Key::Live(LiveTerminalAction::NextTab) => Key::Live(LiveTerminalAction::DirectorNew),
        Key::Live(LiveTerminalAction::DirectorNew) => Key::Live(LiveTerminalAction::NextTab),
        other => other,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum WorkspaceInputRoute {
    Drawer(Vec<Effect>),
    Forwarded,
    Unhandled,
}

fn route_workspace_input_before_reducer(
    ui: &mut WorkspaceUi,
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

/// Pulls the latest safe daemon observation at a TUI redraw boundary.
pub trait MetricsPort {
    /// Compatibility observation hook for simple embedders. Production ports
    /// override [`Self::poll_updates`] to avoid materializing unchanged values.
    fn latest(&mut self) -> Option<DaemonMetrics> {
        None
    }

    /// Compatibility Git snapshot hook. Change-driven ports should override
    /// [`Self::poll_updates`] and move only changed material instead.
    fn git_diffs(&mut self, _sessions: &[(SessionId, PathBuf)]) -> BTreeMap<SessionId, GitDiff> {
        BTreeMap::new()
    }

    /// Return only observations that changed since the previous poll. `sessions`
    /// is supplied only when the daemon session projection changed, so an idle
    /// frame neither clones cwd paths nor rescans active IDs.
    fn poll_updates(
        &mut self,
        sessions: Option<&[(SessionId, PathBuf)]>,
    ) -> Vec<metrics::MetricsUpdate> {
        let mut updates = vec![metrics::MetricsUpdate::Metrics(self.latest())];
        if let Some(sessions) = sessions {
            updates.push(metrics::MetricsUpdate::GitDiffs(self.git_diffs(sessions)));
        }
        updates
    }
}

/// Creates a fresh metrics port for every workspace opened from the screen graph.
pub trait MetricsPortFactory {
    fn create(&mut self) -> Box<dyn MetricsPort>;
}

struct NoMetrics;
impl MetricsPort for NoMetrics {}

struct NoMetricsFactory;
impl MetricsPortFactory for NoMetricsFactory {
    fn create(&mut self) -> Box<dyn MetricsPort> {
        Box::new(NoMetrics)
    }
}

/// Workspace entry ごとに fresh daemon Agent launch port を作る factory。
pub trait AgentCommandPortFactory {
    fn create(&mut self) -> Box<dyn AgentCommandPort>;
}

/// Actions whose stateful host remains in the terminal loop while
/// [`DaemonBackend`] is the sole controller-effect dispatcher.
pub enum ControllerHostAction {
    Create(CreateSessionRequest, Completions),
    Refresh(WorkspaceId, Completions),
    Remove(RemoveSessionRequest, Completions),
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
    pub agent_tab_intents: Box<dyn AgentTabIntentPort>,
    pub external_terminal: Box<dyn ExternalTerminalPort>,
    pub metrics: Box<dyn MetricsPort>,
    pub browser: Box<dyn BrowserOpener>,
    /// Local worktree scan behind the inline create form's collision hint. It
    /// is a port so the frame loop's filesystem IO is countable in a test and
    /// stays out of the frame budget (#554).
    pub session_worktrees: Box<dyn SessionWorktreeScanPort>,
}

/// Dedicated restore-client connection lifecycle observed by the composition
/// root. Epochs are strictly monotonic; duplicate delivery is harmless.
pub trait RestoreConnectionPort: Send {
    fn take_reconnected_epoch(&mut self) -> Option<u64>;
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
    fn load_preview(&mut self, _: Target, completions: Completions) {
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

/// Workspace Config is a Home-owned modal and therefore cannot request that the
/// enclosing TUI exit. Quit chords are projected to [`Self::Stay`] at the modal
/// input boundary.
enum WorkspaceConfigStep {
    Stay,
    Back,
    Save,
}

/// New 画面でキー `key` を処理した結果の遷移。
enum NewStep {
    /// 同じ画面に留まる（フォーム編集を続ける）。
    Stay,
    /// 終了する。
    Quit,
    /// welcome へ戻る。
    Back,
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

/// Open 画面のキー処理結果。
enum OpenStep {
    Stay,
    Quit,
    Back,
    Choose(PathBuf),
    ConfirmCleanup,
    ConfirmUnregister(PathBuf),
}

/// Workspace 画面のキー処理結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceStep {
    /// TUI を終了する。
    Quit,
    /// workspace を離れて Welcome へ戻る。呼び出し側がこの workspace のために
    /// 確立した資源を落としたあと、entry 画面を描き直す（#556）。
    Back,
}

impl WorkspaceStep {
    /// workspace ループの停止理由を TUI 全体の終了理由へ投影する。workspace を
    /// 直接開いた入口（`usagi <path>`）は Welcome を持たないため、合成ルートが
    /// [`Exit::Welcome`] を受けて screen graph へ入り直す。
    const fn exit(self) -> Exit {
        match self {
            Self::Quit => Exit::Quit,
            Self::Back => Exit::Welcome,
        }
    }
}

struct WorkspaceConfigContext<'a> {
    settings: &'a mut dyn SettingsPort,
    available_models: AvailableAgentModels,
}

/// Which Agent CLIs the Closeup `agent` command may select, and which one an
/// omitted `-m` uses.
///
/// Availability is observed by the composition root (the shell owns the PATH
/// probe) and the default comes from the effective settings, so the TUI itself
/// performs no IO to answer either question. The default fits callers without a
/// probe or resolved settings: every CLI offered, `codex` selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AgentModelPolicy {
    available: AvailableAgentModels,
    default: usagi_core::domain::settings::DefaultModel,
}

impl Default for AgentModelPolicy {
    fn default() -> Self {
        Self {
            available: AvailableAgentModels::all(),
            default: usagi_core::domain::settings::DefaultModel::default(),
        }
    }
}

/// Overview の session command を daemon 所有の lifecycle runner へ渡す境界。
///
/// TUI は session store や git worktree を直接操作しない。実行時の合成ルートが
/// daemon IPC client をこの port として注入し、テストは fake port で command と
/// target の対応だけを検証する。
pub trait SessionCommandPort: Send + Sync {
    /// Execute one parsed Overview session command for this workspace and its
    /// currently selected session, when the command requires one.
    ///
    /// # Errors
    ///
    /// Returns a safe message when the daemon cannot accept the request.
    fn execute(
        &self,
        _workspace: &usagi_core::domain::workspace::Workspace,
        _selected: Option<&usagi_core::domain::session::SessionRecord>,
        _command: SessionCommand,
    ) -> Result<SessionCommandResult, String> {
        Err("session command port is not implemented".to_owned())
    }
}

/// Safe result of a daemon-owned session command.
///
/// `sessions` is a read-only projection of the daemon lifecycle snapshot.  It
/// is intentionally returned to the UI instead of being persisted through the
/// legacy workspace state store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCommandResult {
    /// Message for the Overview modal.
    pub message: String,
    /// Authoritative sidebar rows when the daemon supplied a fresh snapshot.
    pub sessions: Option<Vec<usagi_core::domain::session::SessionRecord>>,
    /// Stable daemon identities aligned with [`Self::sessions`].  A lifecycle
    /// refresh must carry these together so a session created during this TUI
    /// run can subsequently launch an Agent without falling back to a name.
    pub session_ids: Option<Vec<SessionId>>,
    /// Safe provider resume state keyed by the same stable session identities.
    pub agent_resumes: Option<BTreeMap<SessionId, ProviderResumeProjection>>,
    /// Safe lifecycle projection keyed by the same stable session identities.
    /// Carries each row's lifecycle (and a `Failed` row's failure summary) so the
    /// sidebar can show state and gate attach/remove by capability.
    pub session_lifecycles: Option<BTreeMap<SessionId, SessionLifecycleProjection>>,
    /// Safe role projection keyed by stable daemon identity. Never persisted.
    pub session_roles: Option<BTreeMap<SessionId, SessionRoleProjection>>,
    /// Monotonically increasing daemon lifecycle revision for this snapshot.
    /// The UI uses it to ignore a response that arrives after a newer command.
    pub revision: Option<u64>,
}

impl SessionCommandResult {
    #[must_use]
    pub fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            sessions: None,
            session_ids: None,
            agent_resumes: None,
            session_lifecycles: None,
            session_roles: None,
            revision: None,
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
    ) -> Result<usagi_core::usecase::client::PrSnapshot, String> {
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

/// Resident session-inventory lane for the Home frame.
///
/// Adopting lifecycle changes another client made (an MCP server creating a
/// session) used to ride on the frame tick: every terminal wake-up dispatched
/// `Effect::RefreshSessions`, which spawned one OS thread that opened a fresh
/// daemon connection. At the composition root's 16ms tick that is a new thread
/// and a new bootstrap per completed round (#551).
///
/// This port replaces that with a worker the composition root keeps resident for
/// the whole workspace: it observes at a bounded cadence on its own persistent
/// connection, coalesces to the newest snapshot, and hands it over through the
/// non-blocking [`take`]. The frame loop only drains.
///
/// [`take`]: Self::take
pub trait SessionRefreshPort: Send {
    /// Ask the resident worker for an immediate out-of-cadence observation, for
    /// a user action that changed lifecycle state and should not wait out the
    /// idle cadence. Never blocks on the daemon.
    fn wake(&mut self) {}

    /// Non-blocking drain of the newest snapshot the worker completed, or `None`
    /// when nothing new arrived since the previous frame.
    fn take(&mut self) -> Option<Result<SessionCommandResult, String>> {
        None
    }
}

/// The lane an embedder that injects no daemon-backed worker gets: it observes
/// nothing, so Home keeps the snapshot it opened with.
struct UnavailableSessionRefreshPort;

impl SessionRefreshPort for UnavailableSessionRefreshPort {}

/// Workspace 起動ごとに Overview の [`SessionCommandPort`] を新しく作る境界。
///
/// screen graph（Welcome→Open / Recent）は 1 ループで複数の workspace を順に開くため、
/// port を都度 fresh に生成して daemon の revision state を workspace 間で持ち越さない。
/// TUI は daemon を知らないので、合成ルートが daemon-backed factory を実装して注入し、
/// テストは fake factory を渡す。
pub trait SessionCommandPortFactory {
    /// Build a fresh session command port for one workspace launch.
    fn create(&mut self) -> Box<dyn SessionCommandPort>;
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
/// [`metrics::MetricsBackend`]. Home row state, input, and rendering belong to
/// the controller (`AppState`/`render_home`), not here.
struct WorkspaceUi {
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
    /// Live coordinator for the active target's selected foreground terminal.
    /// Background and unselected tabs retain only their stable pane identity.
    terminals: Vec<TerminalSession>,
    /// Recently detached coordinators, oldest first. Keeping the coordinator
    /// preserves its connection-local input ledger and unresolved input fence.
    detached_terminals: VecDeque<TerminalSession>,
    terminal_reconnected: bool,
    terminal_size: (usize, usize),
    agent_tab_intent: Option<AgentTabIntentContext>,
    /// A successful durable Reopen requests one fresh coherent daemon
    /// observation. It never projects from an inventory cached before a later
    /// pane admission.
    agent_observation_requested: bool,
    /// An Agent terminal exit changes daemon inventory. Unlike a display-only
    /// observation request, this must schedule one follow-up when an older
    /// restore snapshot is already in flight.
    agent_exit_observation_requested: bool,
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
}

/// One accepted exact-target Agent resume as the daemon answered it (#510).
///
/// Every field is daemon-authoritative: the TUI never infers the lineage or the
/// relation, and a missing relation is refused rather than assumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactAgentResume {
    /// The replacement runtime's new, fully fenced terminal.
    pub terminal: TerminalRef,
    /// Lineage the daemon says the replacement continues.
    pub continuation: Option<AgentContinuationRef>,
    /// Source-to-replacement relation proving which interrupted runtime was
    /// replaced.
    pub relation: Option<AgentResumeRelation>,
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

impl WorkspaceUi {
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
            terminal_reconnected: false,
            terminal_size: (0, 0),
            agent_tab_intent: None,
            agent_observation_requested: false,
            agent_exit_observation_requested: false,
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
    /// Every background target and unselected tab remains detached.
    fn sync_foreground_terminal(&mut self, focused: Option<&TerminalRef>, geometry: Geometry) {
        let stale = self
            .terminals
            .iter()
            .filter(|session| focused.is_none_or(|terminal| !session.terminal().fences(terminal)))
            .map(|session| session.terminal().clone())
            .collect::<Vec<_>>();
        for terminal in stale {
            self.close_terminal(&terminal);
        }
        if let Some(terminal) = focused
            && !self
                .terminals
                .iter()
                .any(|session| session.terminal().fences(terminal))
        {
            self.start_terminal_session(terminal.clone(), geometry);
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

    /// Hand the detached background tabs to the port's bounded scope-inventory
    /// lane and drain the exits it has observed since the last frame.
    ///
    /// This is the whole background contract: metadata only, per scope, off the
    /// render thread. No `Attach` and no terminal-specific `Resume` is ever sent
    /// for a background tab, and the returned refs are exactly the tabs whose
    /// runtime the daemon no longer reports as live.
    fn sync_background_terminals(&mut self, background: &[TerminalRef]) -> Vec<TerminalRef> {
        let Some(agent) = self.agent.as_mut() else {
            return Vec::new();
        };
        agent.port.watch_background_terminals(background);
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
            AgentTabIntentMutation::ObserveAll {
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

    fn request_agent_exit_observation(&mut self) {
        self.agent_exit_observation_requested = true;
    }

    fn take_agent_exit_observation_request(&mut self) -> bool {
        std::mem::take(&mut self.agent_exit_observation_requested)
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

    /// No Agent lineage is hidden: daemon inventory owns visible membership.
    fn agent_dismissed() -> BTreeSet<AgentContinuationRef> {
        BTreeSet::new()
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

    /// Snapshot the retained rows of a detached background terminal without
    /// attaching or polling it. Director owns the one live subscription; this
    /// projection keeps the managed right pane visible behind the drawer.
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
            Key::Left | Key::Char('h') => config.cycle_team_picker(false),
            Key::Right | Key::Char('l') | Key::Tab => config.cycle_team_picker(true),
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
                config.save_environment(settings);
            }
            Key::Enter if config.is_environment_save_focused() => {
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
    }
}

/// Run Config from an opened workspace. The form contains only workspace-owned
/// settings and returns to the still-live Home runtime after Escape or save.
fn run_workspace_config(
    term: &mut dyn Terminal,
    settings: &mut dyn SettingsPort,
    available_models: AvailableAgentModels,
    base: &[String],
) -> io::Result<()> {
    let mut form = Config::load_workspace_with_available_models(settings, available_models);
    loop {
        let (height, width) = term.size()?;
        term.draw(&config::render_over(height, width, base, &form))?;
        match step_workspace_config(&mut form, term.read_key()?, settings) {
            WorkspaceConfigStep::Stay => {}
            WorkspaceConfigStep::Back => return Ok(()),
            WorkspaceConfigStep::Save => {
                play_config_save_wave(term, &mut form, Some(base))?;
                if form.commit_save(settings) {
                    let (height, width) = term.size()?;
                    term.draw(&config::render_over(height, width, base, &form))?;
                    term.wait(config::DONE_DISPLAY)?;
                    form.reset_save();
                    return Ok(());
                }
            }
        }
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
        Key::Tab => {
            form.complete_directory();
            NewStep::Stay
        }
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
            paths
                .into_iter()
                .next()
                .map_or(OpenStep::Stay, OpenStep::Choose)
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
        Key::CtrlD => {
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
        | Key::Resize
        | Key::Other => OpenStep::Stay,
    }
}

/// Run one daemon-owned session command without blocking the terminal event
/// loop. Admission is bounded to one worker; a concurrent request completes as
/// Busy without reaching the shared daemon port.
fn begin_session_command(
    ui: &mut WorkspaceUi,
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
fn session_name_for(ui: &WorkspaceUi, session: SessionId) -> Option<String> {
    ui.workspace
        .session_ids()
        .iter()
        .zip(ui.workspace.sessions())
        .find_map(|(id, record)| (*id == session).then(|| record.name.clone()))
}

/// Reconcile sidebar rows and the IDs used by Agent/terminal requests as one
/// daemon-authoritative observation.  Legacy/test ports may provide rows only;
/// they retain the existing non-runtime projection behaviour.
fn apply_session_projection(
    ui: &mut WorkspaceUi,
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
    if let Some(session_ids) = session_ids.filter(|ids| ids.len() == sessions.len()) {
        ui.workspace
            .replace_sessions_with_runtime_ids(sessions, session_ids.clone());
        if let Some(agent) = ui.agent.as_mut() {
            // Only usable (attachable) sessions can host an Agent. A Failed row
            // owns its name and is now listed, but must never become an Agent
            // launch target, so gate the allowed set by `can_use`. A session with
            // no lifecycle entry (legacy path) stays allowed as before.
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
        ui.workspace.replace_sessions(sessions);
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
fn drain_session_completions(ui: &mut WorkspaceUi) {
    while let Ok(completion) = ui.session_completions.try_recv() {
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
            SessionBackendCompletion::Remove { .. } => {}
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
fn adopt_session_snapshot(ui: &mut WorkspaceUi, result: SessionCommandResult) {
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
    ui: &mut WorkspaceUi,
    session_refresh: &mut dyn SessionRefreshPort,
    pending_session_refresh: &mut Option<Completions>,
) {
    let Some(result) = session_refresh.take() else {
        return;
    };
    match result {
        Ok(result) => {
            let ids = result
                .session_ids
                .clone()
                .unwrap_or_else(|| ui.workspace.session_ids().to_vec());
            adopt_session_snapshot(ui, result);
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
        (Err(message), SessionBackendCompletion::Remove { completions, .. }) => {
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
fn enqueue_pane_launch(ui: &mut WorkspaceUi, launch: PaneLaunch) {
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
fn drain_pane_launches(ui: &mut WorkspaceUi, geometry: Geometry) {
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
            resume,
        } => {
            let result = if resume {
                session.map_or_else(
                    || Err("workspace-root Agent resume is unavailable".to_owned()),
                    |session| port.resume(workspace, session, operation),
                )
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
        // Left/Right move the focus inside a horizontal choice (the Yes/No quit
        // confirmation); the reducer ignores them elsewhere. Tab motion between
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
        // pointer drags and clicks (a shell + `TerminalSession` concern), Ctrl-D
        // (Open Workspace only), and the caret/selection keys that have meaning
        // only inside a focused text field (End/Ctrl-E, Delete, Shift+arrows).
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
        LiveTerminalAction::NextTab => Some(AppKey::CtrlN),
        LiveTerminalAction::PreviousTab => Some(AppKey::CtrlP),
        LiveTerminalAction::OpenPullRequests => Some(AppKey::OpenPrs),
        LiveTerminalAction::Agent => Some(AppKey::CtrlA),
        LiveTerminalAction::Director => Some(AppKey::ToggleDirectorDrawer),
        LiveTerminalAction::DirectorNew => Some(AppKey::OpenDirectorNew),
        LiveTerminalAction::QuitConfirmation => Some(AppKey::OpenQuitConfirmation),
        LiveTerminalAction::CloseTab
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

fn foreground_terminal_geometry(height: usize, width: usize, director_open: bool) -> Geometry {
    if director_open {
        let viewport = director_drawer::terminal_viewport(height, width);
        Geometry {
            cols: u16::try_from(viewport.cols.min(usize::from(u16::MAX)))
                .expect("clamped drawer terminal width fits u16"),
            rows: u16::try_from(viewport.rows.min(usize::from(u16::MAX)))
                .expect("clamped drawer terminal height fits u16"),
        }
    } else {
        terminal_geometry(height, width)
    }
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

/// Recent が指す単体 workspace path。Unite の runtime は今回の対象外なので開かない。
fn recent_path(recent: &Recent) -> Option<&Path> {
    match recent {
        Recent::Workspace(overview) => Some(&overview.workspace.path),
        Recent::Unite(_) => None,
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
fn project_controller_sessions(ui: &WorkspaceUi, state: &AppState) -> Vec<ProjectedSession> {
    #[cfg(test)]
    SESSION_PROJECTION_BUILDS.set(SESSION_PROJECTION_BUILDS.get() + 1);
    let known_sessions = ui
        .workspace
        .session_ids()
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    ui.workspace
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
            projected.role_id = ui
                .workspace
                .session_roles()
                .get(id)
                .and_then(|role| role.role_id.as_ref())
                .map(ToString::to_string);
            if let Some(role) = ui.workspace.session_roles().get(id) {
                projected.parent_session_id = role.parent_session_id;
                projected.organization_depth = 1;
                let mut parent = role.parent_session_id;
                let mut seen = BTreeSet::from([*id]);
                while let Some(parent_id) = parent
                    && known_sessions.contains(&parent_id)
                    && seen.insert(parent_id)
                {
                    projected.organization_depth += 1;
                    parent = ui
                        .workspace
                        .session_roles()
                        .get(&parent_id)
                        .and_then(|projection| projection.parent_session_id);
                }
            }
            if let Some(prs) = state.session_prs(*id) {
                projected.pr_summary = crate::presentation::views::workspace::pr_summary(prs);
            }
            projected
        })
        .collect()
}

/// Render a single static Home frame from a workspace snapshot, using the same
/// controller projection as the interactive loop.
///
/// This is the non-interactive `usagi launch <path>` fallback (no terminal), so
/// it shows the initial Home surface: root selected/active, the snapshot's
/// sessions, and the `+ new session` row.
#[must_use]
pub fn render_home_snapshot(
    height: usize,
    width: usize,
    snapshot: &WorkspaceSnapshot,
) -> Vec<String> {
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
    let projection = HomeProjection::from_state(
        &state,
        &snapshot.workspace.name,
        &snapshot.workspace.path,
        &sessions,
    );
    render_home(height, width, &projection)
}

/// Keep the controller's Home rows in step with the daemon session projection
/// the legacy transport reconciled this frame.
///
/// `worktree_names` is the inline create form's collision hint, supplied by
/// [`SessionWorktreeHint`]. It is empty while the form is closed, because the
/// scan that produces it is filesystem IO which must not ride the frame budget
/// (#554).
fn sync_runtime_sessions(
    runtime: &mut WorkspaceRuntime,
    ui: &WorkspaceUi,
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

/// Preflight scan of the worktree directories which would collide with a new
/// session.
///
/// This is a read-only, best-effort fact for the inline create form. The daemon
/// remains the sole authority that creates or removes worktrees; an unreadable
/// directory simply contributes no local hint and is checked again by the
/// daemon when the user submits the request.
///
/// It is a port because the scan is real filesystem IO. [`SessionWorktreeHint`]
/// keeps it off the frame budget, and injecting it lets a test count exactly
/// how often the frame loop reaches the disk (#554).
pub trait SessionWorktreeScanPort {
    /// Directory names directly under `<workspace>/.usagi/sessions`.
    fn scan(&mut self, workspace: &Path) -> Vec<String>;
}

/// Production scan: one `read_dir` over `<workspace>/.usagi/sessions`.
pub struct FsSessionWorktreeScanPort;

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
    ui: &WorkspaceUi,
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
    let (buffer, row_origin, total_rows) =
        ui.terminal_row_extent(&terminal, controls.selection())?;
    let range = controls.visible_range(buffer, row_origin, total_rows, viewport_rows);
    let rows = ui.terminal_row_window(&terminal, range.start, range.end, controls.selection())?;
    let mut projection = controls.project_window(rows, range.start, total_rows);
    if let Some(error) = ui.terminal_error(&terminal) {
        projection.feedback = Some(error.to_owned());
    }
    Some(projection)
}

/// Project the root pane entry into the frontmost Agent-only drawer.
///
/// Stable identity and selection remain in the pane/intent reducers. This
/// adapter exposes only safe labels and the already-rendered VT rows.
fn director_drawer_projection(
    ui: &WorkspaceUi,
    runtime: &WorkspaceRuntime,
    terminal_view: Option<&TerminalViewProjection>,
) -> DirectorDrawerProjection {
    if !runtime.state().director_drawer_open() {
        return DirectorDrawerProjection::default();
    }
    let pane = runtime.active_pane();
    let selected = pane.selected();
    let mut conversations = Vec::new();
    for tab in pane.tabs() {
        let conversation = match tab {
            PaneTab::Live(live) if live.kind == PaneKind::Agent => Some(DirectorConversation {
                label: AgentTabIntent::safe_label_or_fallback(
                    ui.agent_continuation_for(&live.terminal),
                ),
                selected: matches!(
                    selected,
                    PaneSelection::Tab(TabSelection::Live(terminal))
                        if terminal.fences(&live.terminal)
                ),
            }),
            PaneTab::Interrupted(interrupted) => Some(DirectorConversation {
                label: interrupted.tab.safe_label(),
                selected: matches!(
                    selected,
                    PaneSelection::Tab(TabSelection::Interrupted(continuation))
                        if *continuation == interrupted.tab.continuation
                ),
            }),
            PaneTab::Pending(pending) if pending.kind == PaneKind::Agent => {
                Some(DirectorConversation {
                    label: "Agent (starting)".to_owned(),
                    selected: matches!(
                        selected,
                        PaneSelection::Tab(TabSelection::Pending(operation))
                            if *operation == pending.operation
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
    let feedback = if terminal_view.is_none() {
        pane.error().map(str::to_owned)
    } else {
        None
    };
    DirectorDrawerProjection {
        conversations,
        organization: director_organization(ui),
        terminal_view,
        interrupted_detail,
        feedback,
        new: if runtime.state().director_launching().is_some() {
            DirectorNewProjection::Launching
        } else {
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
                    DirectorNewProjection::Choosing {
                        candidates,
                        selected,
                    }
                }
            }
        },
    }
}

fn director_organization(ui: &WorkspaceUi) -> Vec<DirectorOrganizationRow> {
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
    if members.is_empty() {
        return Vec::new();
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

/// Run the per-frame foreground-terminal sweep: poll the one attached selection,
/// auto-close it if exited, then project its freshly polled viewport. Returns
/// the projection plus its `(rows_len, scroll)` so a later pointer drag maps back
/// to the exact retained cell.
#[cfg(test)]
fn poll_and_project_terminals(
    ui: &mut WorkspaceUi,
    runtime: &mut WorkspaceRuntime,
    controls: &mut LiveTerminalControls,
    geometry: Geometry,
) -> (Option<TerminalViewProjection>, usize, usize) {
    close_exited_panes(ui, runtime);
    let terminal_view = controller_terminal_view(ui, runtime, controls, usize::from(geometry.rows));
    let (rows_len, scroll) = match &terminal_view {
        Some(view) => (view.total_rows, controls.scroll()),
        None => (0, 0),
    };
    (terminal_view, rows_len, scroll)
}

/// Close every pane the daemon reports as exited, from either observation lane:
/// the attached foreground terminal's own `Resume` stream, and the bounded
/// per-scope inventory that watches the detached background tabs. The runtime
/// drops the tab (clearing `has_live_pane` when it was the last) and the shell
/// releases whatever client state it held.
///
/// Both lanes complete on their own threads, so a slow, hung, or unavailable
/// owner delays only the observation, never this frame.
fn close_exited_panes(ui: &mut WorkspaceUi, runtime: &mut WorkspaceRuntime) {
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
        ui.request_agent_exit_observation();
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
        // Workspace root belongs exclusively to the Agent drawer. Generic
        // terminals remain managed-session Closeup panes.
        .filter(|entry| entry.terminal.session_id.is_some())
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
    ui: &mut WorkspaceUi,
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
        &WorkspaceUi::agent_dismissed(),
        &BTreeSet::new(),
    )
    .tabs;
    let observation = match ui.observe_agent_tabs(terminals.clone(), agents) {
        Ok(observation) => observation,
        Err(error) => {
            let targets = generic_restore_targets(workspace, allowed_sessions, &terminals, runtime);
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
        &terminals,
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
fn restore_open_panes(ui: &mut WorkspaceUi, runtime: &mut WorkspaceRuntime, geometry: Geometry) {
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

/// Close the focused pane tab (Ctrl-O x / Ctrl-O Ctrl-X) and perform the daemon transport work
/// the runtime reports: detach a live subscription, or drop a still-pending
/// launch (both its queued work and its completion routing) so it cannot spawn a
/// detached daemon terminal behind the vanished placeholder.
fn close_focused_terminal_pane(
    ui: &mut WorkspaceUi,
    runtime: &mut WorkspaceRuntime,
    pending_targets: &mut std::collections::HashMap<OperationId, Target>,
) {
    // Agent inventory is authoritative and every existing Agent stays visible.
    // Closing a live or interrupted Agent tab would only hide a still-owned
    // runtime and make capacity impossible to manage, so direct the user to
    // terminate the CLI instead. Pending launches remain cancellable below.
    if runtime.focused_agent_terminal().is_some() || runtime.focused_interrupted().is_some() {
        let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::Notice(Notice::new(
            "Agent tabs stay visible; exit the Agent with Ctrl-D",
        ))));
        return;
    }
    let outcome = runtime.close_focused_pane();
    if let Some(terminal) = outcome.detach {
        ui.close_terminal(&terminal);
    }
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
    ui: &WorkspaceUi,
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
        if runtime.state().director_drawer_open() {
            director_drawer::terminal_point_at(height, width, rows_len, scroll, column, row)
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
            let Some(cells) = ui.terminal_cells(&terminal) else {
                return false;
            };
            controls.press_pointer(TerminalSelection::begin(cells, point));
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

fn select_director_tab(key: &Key, ui: &mut WorkspaceUi, runtime: &mut WorkspaceRuntime) -> bool {
    if !runtime.state().director_drawer_open() {
        return false;
    }
    let direction = match key {
        Key::Live(LiveTerminalAction::NextTab) => {
            crate::usecase::application::controller::TabDirection::Next
        }
        Key::Live(LiveTerminalAction::PreviousTab | LiveTerminalAction::OpenPullRequests) => {
            crate::usecase::application::controller::TabDirection::Previous
        }
        _ => return false,
    };
    let Some(selection) = runtime.selection_after_select(direction) else {
        return true;
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
            let _ = runtime.select_tab(direction);
        }
        Err(error) => surface_agent_tab_intent_error(runtime, error),
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
fn visit_garden_agent(ui: &mut WorkspaceUi, runtime: &mut WorkspaceRuntime, click: GardenClick) {
    let GardenClick::Visit {
        agent: Some(runtime_id),
        ..
    } = click
    else {
        return;
    };
    if !runtime.wants_right_pane_tab_click() {
        return;
    }
    let Some(index) = runtime.agent_tab_index(runtime_id, ui.agent_inventory()) else {
        return;
    };
    select_right_pane_tab(ui, runtime, index);
}

fn select_right_pane_tab(ui: &mut WorkspaceUi, runtime: &mut WorkspaceRuntime, index: usize) {
    let Some(selection) = runtime.tab_selection_at(index) else {
        return;
    };
    let active = runtime
        .panes()
        .active()
        .expect("a selectable tab always belongs to an active pane");
    if !ui.has_agent_intent_for(active.session_id()) {
        let _ = runtime.select_tab_selection(selection);
        return;
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
        }
        Err(error) => surface_agent_tab_intent_error(runtime, error),
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
    runtime.state().director_drawer_open()
        && matches!(runtime.state().director_new(), DirectorNew::Idle)
        && runtime.state().director_launching().is_none()
        && director_drawer::new_button_at(height, width, column, row, false)
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
        && director_drawer::new_button_at(height, width, column, row, false)
}

/// Intercept the live-terminal view controls the Home reducer does not own —
/// copy, scroll, tab close, and pointer drag — returning `true` when the key was
/// consumed here so the shell loop skips reducer dispatch. `rows_len` / `scroll`
/// describe the frame's projected viewport for pointer mapping.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn intercept_live_terminal_control(
    key: &Key,
    ui: &mut WorkspaceUi,
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
    if !select_director_tab(key, ui, runtime) {
        match key {
            Key::Live(LiveTerminalAction::ScrollUp) => controls.scroll_up(),
            Key::Live(LiveTerminalAction::ScrollDown) => controls.scroll_down(),
            Key::Live(LiveTerminalAction::ScrollBottom) => controls.scroll_to_bottom(),
            Key::Live(LiveTerminalAction::Wheel { up, column, row }) => {
                let point = if runtime.state().director_drawer_open() {
                    director_drawer::terminal_point_at(height, width, 0, 0, *column, *row)
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
                    Some(encode_mouse_wheel(
                        *up,
                        point.column,
                        point.row,
                        modes.mouse_encoding,
                    ))
                } else if modes.alternate_screen {
                    Some(encode_wheel_arrows(*up, modes.application_cursor))
                } else {
                    None
                };
                if let Some(bytes) = bytes {
                    if let Err(message) = ui.send_terminal_bytes(&terminal, &bytes) {
                        controls.set_feedback(message);
                    }
                } else {
                    for _ in 0..WHEEL_LINES {
                        if *up {
                            controls.scroll_up();
                        } else {
                            controls.scroll_down();
                        }
                    }
                }
            }
            Key::Live(LiveTerminalAction::CloseTab) => {
                close_focused_terminal_pane(ui, runtime, pending_targets);
            }
            Key::Live(LiveTerminalAction::ResumeTab) => {
                resume_focused_interrupted_tab(ui, runtime, pending_targets);
            }
            Key::Live(
                action @ (LiveTerminalAction::MoveTabNext | LiveTerminalAction::MoveTabPrevious),
            ) => {
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
    /// `Some(choice)` exactly while the exit prompt covers the frame, carrying
    /// the answer its focused button would commit (#556).
    quit_confirmation: Option<ExitChoice>,
    /// The create-failure dialog's safe message, present exactly while its
    /// overlay is open. Keying off the message avoids an unreachable "error
    /// overlay without a message" branch.
    create_error: Option<String>,
    /// Failed-delete session label and focused Yes/No answer.
    force_remove_confirmation: Option<(String, bool)>,
    environment_editor: Option<crate::usecase::application::controller::EnvironmentEditor>,
    role_editor: Option<crate::usecase::application::controller::RoleEditor>,
    /// Whole-second wall clock behind the sidebar's relative session times.
    ///
    /// Truncating to the second is what makes time material without making
    /// every frame material: the coarsest thing the renderer derives from it
    /// changes at minute granularity, so a one-second resolution can never be
    /// late, and an idle Home redraws at most once per second because of it.
    now: DateTime<Utc>,
}

/// Cheap dependency vector for the owned Home projection. Equality is the
/// admission gate to projection construction; each revision is advanced by its
/// authoritative controller/daemon source, never by this cache.
#[derive(Debug, PartialEq, Eq)]
struct FrameMaterialKey {
    height: usize,
    width: usize,
    controller: (u64, u64),
    sessions: (u64, Option<SessionId>, u64),
    shell: u64,
    metrics: u64,
    terminal: (u64, u64),
    animation: u64,
    create_pending: Option<String>,
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
            && self.now == other.now
    }
}

impl HomeFrameMaterial {
    fn with_agent_inventory(mut self, inventory: Option<&AgentInventory>) -> Self {
        self.projection = self.projection.with_agent_inventory(inventory);
        self
    }

    fn with_garden_reduced_motion(mut self, reduced_motion: bool) -> Self {
        self.projection = self.projection.with_garden_reduced_motion(reduced_motion);
        self.now = self
            .projection
            .canonical_garden_now(self.height, self.width, self.now);
        self
    }
}

#[allow(clippy::too_many_arguments)]
fn home_frame_material(
    height: usize,
    width: usize,
    runtime: &WorkspaceRuntime,
    workspace_name: &str,
    _root_cwd: &Path,
    sessions: &[ProjectedSession],
    metrics: Option<usagi_core::usecase::client::DaemonMetrics>,
    health: usagi_core::usecase::daemon_health::DaemonHealthTracker,
    git_diffs: &BTreeMap<SessionId, GitDiff>,
    terminal_view: Option<TerminalViewProjection>,
    create_pending: Option<&str>,
    now: DateTime<Utc>,
) -> HomeFrameMaterial {
    home_frame_material_shared(
        height,
        width,
        runtime,
        workspace_name,
        Arc::from(sessions.to_vec()),
        metrics,
        health,
        Arc::new(git_diffs.clone()),
        terminal_view.map(Arc::new),
        create_pending,
        now,
    )
    .with_garden_reduced_motion(false)
}

#[allow(clippy::too_many_arguments)]
fn home_frame_material_shared(
    height: usize,
    width: usize,
    runtime: &WorkspaceRuntime,
    workspace_name: &str,
    sessions: Arc<[ProjectedSession]>,
    metrics: Option<usagi_core::usecase::client::DaemonMetrics>,
    health: usagi_core::usecase::daemon_health::DaemonHealthTracker,
    git_diffs: Arc<BTreeMap<SessionId, GitDiff>>,
    terminal_view: Option<Arc<TerminalViewProjection>>,
    create_pending: Option<&str>,
    now: DateTime<Utc>,
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
    let projection = HomeProjection::from_ordered_state(runtime.state(), workspace_name, sessions)
        .with_pane(runtime.preview_pane())
        .with_metrics(metrics)
        // Diagnostic-only material. It rides the frame material like every
        // other renderer input, so an idle Home still skips redraws.
        .with_health(health)
        .with_shared_git_diffs(git_diffs)
        .with_shared_terminal_view(terminal_view)
        .with_director_drawer(runtime.director_projection().clone())
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
        quit_confirmation: (runtime.state().overlay() == Some(Overlay::QuitConfirmation))
            .then(|| runtime.state().exit_choice()),
        create_error: runtime
            .state()
            .create_session_error()
            .map(|error| error.message.clone()),
        force_remove_confirmation,
        environment_editor: runtime.state().environment_editor().cloned(),
        role_editor: runtime.state().role_editor().cloned(),
        // Garden canonicalization happens only after every composition-owned
        // source (notably Agent inventory and reduced motion) is attached.
        now: now.with_nanosecond(0).unwrap_or(now),
    }
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
    root_cwd: &Path,
    sessions: &[ProjectedSession],
    metrics: Option<usagi_core::usecase::client::DaemonMetrics>,
    health: usagi_core::usecase::daemon_health::DaemonHealthTracker,
    git_diffs: &BTreeMap<SessionId, GitDiff>,
    terminal_view: Option<TerminalViewProjection>,
    create_pending: Option<&str>,
) -> Vec<String> {
    render_home_material(&home_frame_material(
        height,
        width,
        runtime,
        workspace_name,
        root_cwd,
        sessions,
        metrics,
        health,
        git_diffs,
        terminal_view,
        create_pending,
        Utc::now(),
    ))
}

/// Apply actions already routed by [`DaemonBackend`] to the stateful terminal
/// host. This layer owns no Effect matching and therefore cannot diverge from
/// the backend's route matrix.
#[allow(clippy::too_many_lines)]
fn drain_controller_host_actions(
    actions: &Receiver<ControllerHostAction>,
    ui: &mut WorkspaceUi,
    runtime: &mut WorkspaceRuntime,
    pending_targets: &mut std::collections::HashMap<OperationId, Target>,
    session_refresh: &mut dyn SessionRefreshPort,
    pending_session_refresh: &mut Option<Completions>,
) {
    while let Ok(action) = actions.try_recv() {
        match action {
            ControllerHostAction::Create(request, completions) => {
                let name = request.intent.name;
                let role_id = request.intent.role_id;
                let before = ui.workspace.session_ids().to_vec();
                if begin_session_command(
                    ui,
                    SessionCommand::Create {
                        name: name.clone(),
                        role_id,
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
            ControllerHostAction::LaunchAgent(request) => {
                let target = request
                    .session
                    .map_or(Target::Root(request.workspace), Target::Session);
                pending_targets.insert(request.operation_id, target);
                runtime.on_effect(&Effect::LaunchAgent {
                    workspace: request.workspace,
                    session: request.session,
                    operation_id: request.operation_id,
                    profile: request.profile.clone(),
                });
                enqueue_pane_launch(
                    ui,
                    PaneLaunch::Agent {
                        operation: request.operation_id,
                        workspace: request.workspace,
                        session: request.session,
                        profile: request.profile,
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
                // The workspace root pane is the Agent-only drawer. Refuse a
                // generic terminal before recording a placeholder or issuing
                // daemon work; managed-session Closeup remains unchanged.
                if matches!(request.target, Target::Root(_)) {
                    let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::Notice(
                        Notice::new(format!(
                            "{} Director accepts Agent conversations only",
                            director_drawer::DIRECTOR_ICON
                        )),
                    )));
                    continue;
                }
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
                            let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::Notice(
                                Notice::new(error),
                            )));
                        }
                    }
                    None => {
                        let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::Notice(
                            Notice::new("selected session is no longer available"),
                        )));
                    }
                }
            }
            ControllerHostAction::SelectTab(direction) => {
                let Some(active) = runtime.panes().active() else {
                    continue;
                };
                let Some(next_terminal) = runtime.terminal_after_select(direction) else {
                    continue;
                };
                if !ui.has_agent_intent_for(active.session_id()) {
                    runtime.on_effect(&Effect::SelectTab { direction });
                    continue;
                }
                let continuation = next_terminal
                    .as_ref()
                    .and_then(|terminal| ui.agent_continuation_for(terminal));
                match ui.mutate_agent_intent(AgentTabIntentMutation::Select {
                    session_id: active.session_id(),
                    continuation,
                }) {
                    Ok(()) => runtime.on_effect(&Effect::SelectTab { direction }),
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
    ui: &mut WorkspaceUi,
    runtime: &mut WorkspaceRuntime,
    pending_targets: &mut std::collections::HashMap<OperationId, Target>,
    _geometry: Geometry,
) {
    while let Ok(completion) = ui.pane_completions.try_recv() {
        if ui.active_pane_launch == Some(completion.launch_id) {
            ui.active_pane_launch = None;
        }
        match completion.outcome {
            PaneLaunchOutcome::Agent { operation, result } => {
                let Some(target) = pending_targets.remove(&operation) else {
                    continue;
                };
                match result {
                    Ok(admission) => {
                        let terminal = admission.terminal;
                        if let Some(continuation) = admission.continuation {
                            // A confirmed drawer New always becomes the root
                            // conversation selection. This durable selection is
                            // committed atomically with its order slot before
                            // the pending runtime is promoted. Managed-session
                            // launches retain the existing no-focus-steal gate.
                            let select = matches!(target, Target::Root(_))
                                || runtime.pane_completion_will_focus(operation);
                            match ui.mutate_agent_intent(AgentTabIntentMutation::Upsert {
                                session_id: target.session_id(),
                                continuation,
                                terminal: terminal.clone(),
                                select,
                            }) {
                                Ok(()) => {
                                    let _ = runtime.complete_pane_focus_if_uninterrupted(
                                        target, operation, terminal,
                                    );
                                }
                                Err(error) => {
                                    let _ = runtime.fail_pane(
                                        target,
                                        operation,
                                        error.safe_message().to_owned(),
                                    );
                                    surface_agent_tab_intent_error(runtime, error);
                                }
                            }
                        } else if matches!(target, Target::Root(_)) && ui.agent_tab_intent.is_some()
                        {
                            // A production root conversation must have the
                            // daemon-issued continuation needed to atomically
                            // persist its order/selection. Compatibility
                            // embedders without an intent port retain their
                            // pre-intent pane behaviour.
                            let _ = runtime.fail_pane(
                                target,
                                operation,
                                "daemon did not return a root Agent conversation".to_owned(),
                            );
                        } else {
                            let _ = runtime
                                .complete_pane_focus_if_uninterrupted(target, operation, terminal);
                        }
                    }
                    Err(message) => {
                        let _ = runtime.fail_pane(target, operation, message);
                    }
                }
                if matches!(target, Target::Root(_)) {
                    let _ = runtime.apply_event(AppEvent::DirectorLaunchFinished(operation));
                }
            }
            PaneLaunchOutcome::ResumeExact {
                operation,
                continuation,
                result,
            } => {
                let Some(target) = pending_targets.remove(&operation) else {
                    continue;
                };
                apply_exact_resume(ui, runtime, target, operation, continuation, result);
            }
            PaneLaunchOutcome::Terminal { operation, result } => {
                let Some(target) = pending_targets.remove(&operation) else {
                    continue;
                };
                match result {
                    Ok(terminal) => {
                        let _ = runtime
                            .complete_pane_focus_if_uninterrupted(target, operation, terminal);
                    }
                    Err(message) => {
                        let _ = runtime.fail_pane(target, operation, message);
                    }
                }
            }
        }
    }
}

/// Apply one explicit per-tab resume answer (#510).
///
/// The runtime validates the answer against the exact interrupted tab before any
/// tab changes; only an accepted replacement turns that one tab live, and its new
/// terminal is recorded as #506 display intent so the next observation keeps it.
/// Every refusal leaves the interrupted tab in place with safe feedback.
fn apply_exact_resume(
    ui: &mut WorkspaceUi,
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

/// Start the explicit resume of the selected interrupted tab (`Ctrl-O r`).
///
/// This is the only path that asks the daemon to resume a provider conversation
/// per tab: the request carries the daemon's own opaque target plus a fresh
/// durable operation, and it marks exactly that tab pending.
fn resume_focused_interrupted_tab(
    ui: &mut WorkspaceUi,
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

/// Controller-driven real-terminal frame loop (`drain → poll → render → input →
/// dispatch`). Home row state, live-pane availability, and the Home frame come
/// from [`WorkspaceRuntime`]/`render_home`; the legacy [`WorkspaceUi`] is kept as
/// the daemon IO transport (session workers, pane launches, terminal streams,
/// metrics). This is the controller replacement for
/// `drive_workspace_with_agent_port_and_selection_mode`; the composition root
/// switches to it separately.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
#[coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=screen_graph_production_port_harness
fn drive_workspace_controller(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    backend_factory: &mut dyn ControllerBackendFactory,
    modal_selection_mode: usagi_core::domain::settings::ModalSelectionMode,
    pr_auto_open: usagi_core::domain::settings::PrAutoOpen,
    agent_models: AgentModelPolicy,
    mut workspace_config: Option<WorkspaceConfigContext<'_>>,
) -> io::Result<WorkspaceStep> {
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
    let mut workspace =
        WorkspaceView::with_runtime_ids(snapshot.workspace, snapshot.state, session_ids.clone());
    workspace.set_session_lifecycles(session_lifecycles);
    let mut ui = WorkspaceUi::new(workspace, composition.session_commands)
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
    runtime.set_pr_auto_open(pr_auto_open);
    let data_home = usagi_core::infrastructure::paths::data_dir().ok();
    let role_catalog = session_role_catalog(data_home.as_deref(), &root_cwd);
    let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::SessionRoleCatalog(
        role_catalog,
    )));
    runtime.set_agent_models(agent_models.available, agent_models.default);
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
    let mut director_material_key = None;
    let mut frame_material_key: Option<FrameMaterialKey> = None;
    loop {
        for event in backend.drain_events() {
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
        if ui.take_agent_exit_observation_request() {
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
        let current_sessions = ui
            .workspace
            .session_ids()
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        ui.set_allowed_agent_sessions(current_sessions.iter().copied());
        while let Ok(completion) = restore_completions.try_recv() {
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
        let (height, width) = term.size()?;
        ui.set_terminal_size(height, width);
        let _ = runtime.apply_event(AppEvent::Resize {
            width: u16::try_from(width).unwrap_or(u16::MAX),
            height: u16::try_from(height).unwrap_or(u16::MAX),
        });
        let geometry =
            foreground_terminal_geometry(height, width, runtime.state().director_drawer_open());
        drain_pane_completions_into_runtime(&mut ui, &mut runtime, &mut pending_targets, geometry);
        // The right pane is what the foreground attachment serves, so it follows
        // the previewed terminal: Switch's hover, Closeup's focus.
        ui.sync_foreground_terminal(runtime.preview_terminal().as_ref(), geometry);
        ui.resize_terminals(geometry);
        // Polling still runs every tick so output/admission progresses, but row
        // String creation and URL scanning run only behind the projection key.
        close_exited_panes(&mut ui, &mut runtime);
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
            focused_terminal,
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
        let background_terminal = runtime.director_background_terminal();
        let background_revision = background_terminal
            .as_ref()
            .and_then(|terminal| ui.terminal_projection_key(terminal))
            .unwrap_or(0);
        let background_rows = workspace::terminal_viewport(height, width).0;
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
        let next_director_key = (
            runtime.material_key(),
            ui.material_revision,
            terminal_generation,
        );
        if director_material_key != Some(next_director_key) {
            let drawer_projection =
                director_drawer_projection(&ui, &runtime, terminal_view.as_deref());
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
            metrics_sessions = sessions
                .iter()
                .map(|session| (session.id, session.cwd.clone()))
                .collect();
            session_material_key = Some(next_session_key);
        }
        // Reflux daemon metrics / git diffs through the backend drain instead of
        // polling the port inline: the shell folds the updates into its own
        // projection cache, so the material no longer rides on the legacy view.
        metrics_backend.poll(sessions_changed.then_some(metrics_sessions.as_slice()));
        for update in metrics_backend.drain_events() {
            metrics_projection.apply(update);
        }
        let now = Utc::now();
        let now = now.with_nanosecond(0).unwrap_or(now);
        let drives_tick_animation = ui.creating_session.is_some()
            || sessions.iter().any(|session| session.removing)
            || runtime
                .preview_pane()
                .tabs()
                .iter()
                .any(|tab| matches!(tab, PaneTab::Pending(_)));
        let animation = if drives_tick_animation {
            runtime.state().mascot_tick()
        } else {
            widgets::mascot::canonical_tick(runtime.state().mascot_tick())
        };
        let next_frame_key = FrameMaterialKey {
            height,
            width,
            controller: runtime.material_key(),
            sessions: next_session_key,
            shell: ui.material_revision,
            metrics: metrics_projection.generation(),
            terminal: (terminal_generation, background_terminal_generation),
            animation,
            create_pending: ui
                .creating_session
                .as_ref()
                .map(|create| create.name.clone()),
            now,
        };
        if frame_material_key.as_ref() != Some(&next_frame_key) {
            let controller_may_be_noop = frame_material_key
                .as_ref()
                .is_some_and(|previous| previous.differs_only_by_controller(&next_frame_key));
            let material = home_frame_material_shared(
                height,
                width,
                &runtime,
                &workspace_name,
                Arc::clone(&sessions),
                metrics_projection.metrics(),
                metrics_projection.health(),
                metrics_projection.shared_git_diffs(),
                if runtime.state().director_drawer_open() {
                    background_terminal_view.as_ref().map(Arc::clone)
                } else {
                    terminal_view.as_ref().map(Arc::clone)
                },
                ui.creating_session
                    .as_ref()
                    .map(|create| create.name.as_str()),
                now,
            )
            .with_agent_inventory(ui.agent_inventory())
            .with_garden_reduced_motion(garden_reduced_motion);
            // Skip only the drawing. A skipped tick has already run every drain
            // above and still runs restore admission, pane launches, and input
            // below, so nothing that makes progress depends on the redraw.
            if !controller_may_be_noop || drawn_material.as_ref() != Some(&material) {
                term.draw(&render_home_material(&material))?;
                drawn_material = Some(material);
            }
            frame_material_key = Some(next_frame_key);
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
        // Director mode owns `Ctrl-O Ctrl-N` as New; the swap happens once here
        // so PTY forwarding, pane controls, and the reducer all see one key.
        let key = retarget_director_chords(&runtime, term.read_key()?);
        // Screen saver admission, before the key is routed anywhere: a wake-up
        // key resets the deadline first, so the frame that wakes the user can
        // never re-open the garden it just closed. A terminal too small to draw
        // a garden emits no idle event at all, leaving its usable Home alone.
        let idle = idle_watch.observe(&key, pointer_clock.elapsed());
        if garden_fits(height, width) {
            let _ = runtime.apply_event(AppEvent::IdleElapsed(idle));
        }
        // Neither a tick nor a resize refreshes an inventory here any more. Both
        // used to dispatch `RefreshDecisions` + `RefreshSessions`, which ran the
        // daemon round trip on this thread at the 16ms frame cadence; the
        // decision and session lanes are now resident background workers with
        // their own bounded cadence, drained at the head of this loop (#551).
        // A tick and a resize therefore cost exactly one redraw each, and the
        // only wake left is the explicit one a lifecycle action asks for through
        // `ControllerHostAction`.
        let input_route =
            route_workspace_input_before_reducer(&mut ui, &mut runtime, &mut controls, term, &key);
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
        let daemon_overlay_was_open = runtime.state().overlay() == Some(Overlay::Daemon);
        let effects = if let WorkspaceInputRoute::Drawer(effects) = input_route {
            effects
        } else if is_director_new_click(&key, &runtime, height, width) {
            runtime.apply_event(AppEvent::Key(AppKey::OpenDirectorNew))
        } else if let Key::Click { column, row } = key {
            // The garden owns the whole frame while it is up, and it resolves
            // its own clicks: the layout call that drew the rabbits returns the
            // `SessionId`-tagged plots, so the shell hit-tests the frame the
            // user actually saw instead of re-deriving session order from cells.
            let garden_click = drawn_material.as_ref().and_then(|material| {
                garden_click_at(
                    material.height,
                    material.width,
                    &material.projection,
                    material.now,
                    column,
                    row,
                )
            });
            // Header rendering and hit-testing share one layout projection, so
            // CJK breadcrumbs, notice presence, and narrow clipping cannot move
            // an action away from its clickable cells.
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
            match (garden_click, header_action, pane_tab) {
                (Some(click), _, _) => {
                    let effects = runtime.apply_event(AppEvent::GardenClick(click));
                    // The activation this event performed made the clicked
                    // session's pane the active one, so the rabbit's own tab can
                    // be selected now. A rabbit whose tab has meanwhile gone
                    // leaves the plain session Closeup as it is.
                    visit_garden_agent(&mut ui, &mut runtime, click);
                    effects
                }
                (None, Some(HomeHeaderAction::Director), _) => {
                    runtime.apply_event(AppEvent::Key(AppKey::ToggleDirectorDrawer))
                }
                (None, Some(HomeHeaderAction::Decisions), _) => {
                    runtime.apply_event(AppEvent::Key(AppKey::OpenDecisions))
                }
                (None, None, Some(index)) => {
                    select_right_pane_tab(&mut ui, &mut runtime, index);
                    Vec::new()
                }
                (None, None, None) => {
                    runtime.apply_event(sidebar_pointer_event(column, row, pointer_clock.elapsed()))
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
                let base = render_controller_frame(
                    height,
                    width,
                    &runtime,
                    &workspace_name,
                    &root_cwd,
                    &sessions,
                    metrics_projection.metrics(),
                    metrics_projection.health(),
                    metrics_projection.git_diffs(),
                    terminal_view,
                    ui.creating_session
                        .as_ref()
                        .map(|create| create.name.as_str()),
                );
                run_workspace_config(term, context.settings, context.available_models, &base)?;
                // The modal drew over the frame the gate remembers, so the next
                // tick must redraw even if no material changed underneath it.
                drawn_material = None;
                frame_material_key = None;
                let effective =
                    usagi_core::usecase::settings::read_for_workspace_entry(context.settings);
                runtime.set_modal_selection_mode(effective.modal_selection_mode);
                runtime.set_pr_auto_open(effective.pr_auto_open);
                // A newly saved Agent default applies to the next `agent`
                // command without reopening the workspace.
                runtime.set_agent_models(context.available_models, effective.default_model);
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
    drive_workspace_controller(
        term,
        snapshot,
        backend_factory,
        usagi_core::domain::settings::ModalSelectionMode::Action,
        usagi_core::domain::settings::PrAutoOpen::default(),
        AgentModelPolicy::default(),
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
    drive_workspace_controller(
        term,
        snapshot,
        backend_factory,
        settings.modal_selection_mode,
        settings.pr_auto_open,
        AgentModelPolicy {
            default: settings.default_model,
            ..AgentModelPolicy::default()
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
    drive_workspace_controller(
        term,
        snapshot,
        backend_factory,
        effective.modal_selection_mode,
        effective.pr_auto_open,
        AgentModelPolicy {
            available: available_models,
            default: effective.default_model,
        },
        Some(WorkspaceConfigContext {
            settings,
            available_models,
        }),
    )
    .map(WorkspaceStep::exit)
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
                .unwrap_or_else(|| Box::new(FsSessionWorktreeScanPort)),
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

/// Run the Welcome / Open / Recent graph with the daemon Agent launch factory.
///
/// # Errors
///
/// Returns workspace loading or terminal IO failures from the screen graph.
#[allow(clippy::too_many_arguments)]
#[coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=screen_graph_production_port_harness
pub fn run_with_settings_and_agent_port_factory(
    term: &mut dyn Terminal,
    workspaces: Vec<Workspace>,
    recent: Vec<Recent>,
    now: DateTime<Utc>,
    start: Start,
    loader: &mut dyn WorkspaceLoader,
    settings: &mut dyn SettingsPort,
    session_commands: &mut dyn SessionCommandPortFactory,
    agent_commands: &mut dyn AgentCommandPortFactory,
) -> io::Result<Exit> {
    run_with_settings_and_agent_port_factory_and_model_availability(
        term,
        workspaces,
        recent,
        now,
        start,
        loader,
        settings,
        session_commands,
        agent_commands,
        AvailableAgentModels::all(),
    )
}

/// Run the screen graph while limiting Config's Agent model choices to installed CLIs.
///
/// # Errors
///
/// Returns workspace loading or terminal IO failures from the screen graph.
#[allow(clippy::too_many_arguments)]
#[coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=screen_graph_production_port_harness
pub fn run_with_settings_and_agent_port_factory_and_model_availability(
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
) -> io::Result<Exit> {
    let mut metrics = NoMetricsFactory;
    run_with_settings_and_agent_and_metrics_port_factory_and_model_availability(
        term,
        workspaces,
        recent,
        now,
        start,
        loader,
        settings,
        session_commands,
        agent_commands,
        available_models,
        &mut metrics,
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
fn open_snapshot_via_controller(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    settings: &mut dyn SettingsPort,
    backend_factory: &mut dyn ControllerBackendFactory,
    available_models: AvailableAgentModels,
) -> io::Result<WorkspaceStep> {
    settings.select_workspace(&snapshot.workspace.path)?;
    let effective = usagi_core::usecase::settings::read_for_workspace_entry(settings);
    drive_workspace_controller(
        term,
        snapshot,
        backend_factory,
        effective.modal_selection_mode,
        effective.pr_auto_open,
        AgentModelPolicy {
            available: available_models,
            default: effective.default_model,
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
    settings: &mut dyn SettingsPort,
    backend_factory: &mut dyn ControllerBackendFactory,
    available_models: AvailableAgentModels,
) -> io::Result<Option<Exit>> {
    let step =
        open_snapshot_via_controller(term, snapshot, settings, backend_factory, available_models)?;
    Ok(match step {
        WorkspaceStep::Quit => Some(Exit::Quit),
        WorkspaceStep::Back => None,
    })
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
            agent_tab_intents: Box::new(UnavailableAgentTabIntentPort),
            external_terminal: Box::new(UnavailableExternalTerminalPort),
            metrics,
            browser: Box::new(UnavailableBrowserOpener),
            session_worktrees: Box::new(FsSessionWorktreeScanPort),
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
        }
    }

    fn render(&self, now: DateTime<Utc>) -> Vec<String> {
        match &self.form {
            EntryForm::Welcome(welcome) => welcome::render(self.height, self.width, welcome, now),
            EntryForm::Open(open) => render_open(self.height, self.width, open, now),
            EntryForm::New(form) => new::render(self.height, self.width, form),
            EntryForm::Config(form) => config::render(self.height, self.width, form),
        }
    }
}

/// Production screen graph entry. Every Welcome/Open/Recent/New path creates
/// its workspace runtime through the same backend factory as direct launch.
///
/// # Errors
///
/// Returns workspace loading, settings, or terminal IO failures.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
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
    let mut welcome = Welcome::new(recent);
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
    let mut next_create_token = 1_u64;
    let mut pending_create: Option<PendingWorkspaceCreate> = None;
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
            welcome.record_opened(&snapshot.workspace);
            open.record_opened(&snapshot.workspace);
            if let Some(exit) =
                enter_workspace(term, snapshot, settings, backend_factory, available_models)?
            {
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
        );
        if drawn_material.as_ref() != Some(&material) {
            term.draw(&material.render(now))?;
            drawn_material = Some(material);
        }
        let key = term.read_key()?;
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
                    let Some(path) = welcome
                        .recent()
                        .get(index)
                        .and_then(recent_path)
                        .map(Path::to_path_buf)
                    else {
                        continue;
                    };
                    // A workspace this daemon does not serve keeps the switcher on
                    // screen with the reason, so another Recent entry can be tried.
                    let snapshot = match loader.open(&path) {
                        Ok(snapshot) => snapshot,
                        Err(error) => match open_refusal_notice(&error) {
                            Some(notice) => {
                                welcome.set_notice(Some(notice));
                                continue;
                            }
                            None => return Err(error),
                        },
                    };
                    welcome.set_notice(None);
                    welcome.record_opened(&snapshot.workspace);
                    open.record_opened(&snapshot.workspace);
                    if let Some(exit) = enter_workspace(
                        term,
                        snapshot,
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
                OpenStep::Choose(path) => {
                    // Same contract as Recent: the list stays up with the reason so
                    // the workspace this daemon does serve can be chosen instead.
                    let snapshot = match loader.open(&path) {
                        Ok(snapshot) => snapshot,
                        Err(error) => match open_refusal_notice(&error) {
                            Some(notice) => {
                                open.set_notice(Some(notice));
                                continue;
                            }
                            None => return Err(error),
                        },
                    };
                    open.set_notice(None);
                    welcome.record_opened(&snapshot.workspace);
                    open.record_opened(&snapshot.workspace);
                    // Leaving returns to Welcome, not to the list that was used
                    // to get here: all three entries share one way back so the
                    // switcher is always reachable from a workspace.
                    if let Some(exit) = enter_workspace(
                        term,
                        snapshot,
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
                }
                OpenStep::ConfirmUnregister(path) => {
                    let removed = loader.unregister(&[path])?;
                    open.remove_paths(&removed);
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
                    play_config_save_wave(term, &mut config_form, None)?;
                    if config_form.commit_save(settings) {
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
mod tests {
    #![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract
    use super::{
        AgentCommandPort, AgentCommandPortFactory, AgentPaneAdmission, AgentTabIntentPort,
        AgentTabIntentPortCommit, BTreeMap, BannerScreenRunner, BrowserOpener, Config, ConfigStep,
        ControllerHost, ControllerHostAction, DecisionCommandPort, DefaultSettingsPort,
        DesktopNotificationPort, EnvironmentStorePort, Exit, ExternalTerminalPort,
        FixedBackendFactory, FsSessionWorktreeScanPort, Geometry, GitDiff, IdleWatch,
        MAX_BACKGROUND_EXITS_PER_FRAME, MetricsPort, MetricsPortFactory, NewStep,
        NoDesktopNotifications, NoMetrics, NoMetricsFactory, OpenStep, PaneLaunch,
        PaneLaunchCommandPort, ProjectedSession, SerializedPaneLaunchPort, SessionCommandPort,
        SessionCommandPortFactory, SessionCommandResult, SessionLifecycle,
        SessionLifecycleProjection, SessionRefreshPort, SessionWorktreeHint,
        SessionWorktreeScanPort, Start, TerminalAttach, TerminalChunk, TerminalError,
        TerminalInputOutcome, TerminalInputResolution, TerminalSubscription,
        TerminalViewProjection, UnavailableAgentCommandPort, UnavailableBackendPort,
        UnavailableBrowserOpener, UnavailableDecisionCommandPort, UnavailableEnvironmentStore,
        UnavailableExternalTerminalPort, UnavailablePaneLaunchPort, UnavailablePrSnapshotPort,
        UnavailableSessionCommandPort, UnavailableSessionCommandPortFactory, WelcomeStep,
        WorkspaceCreateCompletion, WorkspaceCreateEffect, WorkspaceCreateToken,
        WorkspaceInputRoute, WorkspaceLoader, WorkspaceRuntime, WorkspaceSnapshot, WorkspaceUi,
        WorkspaceView, app_event_from_key, close_exited_panes, controller_terminal_view,
        copy_terminal_selection, director_organization, drain_session_completions,
        foreground_terminal_geometry, forward_live_terminal_input, garden_click_at,
        handle_terminal_pointer, home_frame_material, intercept_live_terminal_control,
        is_user_activity, key_to_terminal_bytes, new_project_notice, play_startup_splash,
        poll_and_project_terminals, projection_build_counts, render_controller_frame,
        render_home_material, render_home_snapshot, reset_projection_build_counts,
        restore_open_panes, retarget_director_chords, route_workspace_input_before_reducer,
        run as run_from_start, run_screen_graph_with_backend, run_with_settings,
        run_with_settings_and_agent_and_metrics_port_factory_and_model_availability,
        run_workspace_config, run_workspace_controller, run_workspace_controller_with_backend,
        run_workspace_controller_with_backend_and_config,
        run_workspace_controller_with_backend_and_settings, safe_session_error,
        select_right_pane_tab, sidebar_pointer_event, step_config, step_new, step_open,
        terminal_geometry, visit_garden_agent, welcome_action, write_banner,
    };
    use crate::presentation::live_terminal::LiveTerminalControls;
    use crate::presentation::views::config::AvailableAgentModels;
    use crate::presentation::views::new::{Field, Mode, New};
    use crate::presentation::views::open::Open;
    use crate::presentation::views::welcome::MenuAction;
    use crate::presentation::widgets::strip_ansi;
    use crate::presentation::workspace_runtime::PaneRestoreTarget;
    use crate::usecase::application::agent_tab_intent::{
        AgentTabIntent, AgentTabIntentError, AgentTabIntentMutation, AgentTabProjection,
        AgentTabSlotIntent, AgentTabTargetProjection,
    };
    use crate::usecase::application::controller::{
        AppEvent, AppKey, BackendEvent, DirectorNew, Effect, EnvironmentEntry,
        GARDEN_IDLE_THRESHOLD, GardenClick, HomeMode, NewRequest, Overlay, PendingToken,
        RoleEditorScope, Route, SessionCreateIntent, SessionRoleCatalog, TabDirection, Target,
    };
    use crate::usecase::application::daemon_backend::{
        Completions, DaemonBackend, DecisionPort as BackendDecisionPort, ReopenAgentRequest,
    };
    use crate::usecase::application::pane::{LivePane, PaneKind, PaneTab, TabSelection};
    use crate::usecase::application::pr::PrSnapshotPort;
    use crate::usecase::application::run as dispatch;
    use crate::usecase::application::terminal_selection::{TerminalPoint, TerminalSelection};
    use crate::usecase::application::{EntryScreen, Key, Terminal};
    use crate::usecase::overview::SessionCommand;
    use crate::usecase::terminal_input::{LiveTerminalAction, PointerEvent, PointerKind};
    use chrono::{DateTime, Duration, Timelike, Utc};
    use std::collections::{BTreeSet, VecDeque};
    use std::io::{self, Write};
    use std::path::{Path, PathBuf};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{Receiver, Sender},
    };
    use usagi_core::domain::AppInfo;
    use usagi_core::domain::agent::{
        AgentInventory, AgentProfileId, AgentRuntimeInventoryItem, AgentRuntimeInventoryState,
    };
    use usagi_core::domain::id::{
        AgentContinuationRef, AgentRuntimeId, AgentRuntimeRef, DaemonGeneration, OperationId,
        SessionId, TerminalId, TerminalRef, UserDecisionId, WorkspaceId, WorktreeId,
    };
    use usagi_core::domain::note::Scratchpad;
    use usagi_core::domain::settings::{AvailableModels, DefaultModel, Settings};
    use usagi_core::domain::terminal_launch::{TerminalInventoryEntry, TerminalKind};
    use usagi_core::domain::user_decision::UserDecisionAnswer;
    use usagi_core::usecase::env::EnvScope;
    use usagi_core::usecase::settings::{SettingsPort, SettingsScope};

    use usagi_core::domain::recent::{Recent, UniteOverview};
    use usagi_core::domain::session::{SessionOrigin, SessionRecord};

    use tempfile::tempdir;
    use usagi_core::domain::workspace::{Workspace, WorkspaceOverview};
    use usagi_core::domain::workspace_state::WorkspaceState;
    use usagi_core::usecase::client::DaemonMetrics;
    use usagi_core::usecase::daemon_health::DaemonHealthTracker;

    /// The unobserved default: diagnostic health draws no indicator, so a frame
    /// test keeps asserting the healthy Home frame. The judgement itself is
    /// covered by `usagi_core::usecase::daemon_health` and by the sidecar tests
    /// in [`views::workspace`].
    fn health() -> DaemonHealthTracker {
        DaemonHealthTracker::default()
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-06-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    /// Host-action routing without the resident session lane. These tests
    /// exercise which action a dispatched effect produces; the lane's own
    /// behaviour is covered by [`FakeSessionRefreshPort`] and the frame-loop
    /// tests below.
    fn drain_host_actions(
        actions: &Receiver<ControllerHostAction>,
        ui: &mut WorkspaceUi,
        runtime: &mut WorkspaceRuntime,
        pending_targets: &mut std::collections::HashMap<OperationId, Target>,
    ) {
        super::drain_controller_host_actions(
            actions,
            ui,
            runtime,
            pending_targets,
            &mut super::UnavailableSessionRefreshPort,
            &mut None,
        );
    }

    /// A scripted resident session-inventory lane. It records every wake and
    /// hands over queued snapshots the way the daemon-backed lane hands over
    /// what its worker already fetched — so a test can assert the frame loop
    /// only ever drains, and count what the lane was asked to do (#551).
    #[derive(Default)]
    struct FakeSessionRefreshPort {
        wakes: Arc<AtomicUsize>,
        takes: Arc<AtomicUsize>,
        queued: Arc<Mutex<VecDeque<Result<SessionCommandResult, String>>>>,
    }

    impl SessionRefreshPort for FakeSessionRefreshPort {
        fn wake(&mut self) {
            self.wakes.fetch_add(1, Ordering::SeqCst);
        }

        fn take(&mut self) -> Option<Result<SessionCommandResult, String>> {
            self.takes.fetch_add(1, Ordering::SeqCst);
            self.queued
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
        }
    }

    /// Publish one daemon snapshot on an exact drain observation. The frame
    /// loop itself advances `takes`; no wall-clock delay or worker scheduling is
    /// involved, so cache invalidation tests can put the change between two
    /// already-rendered frames deterministically.
    struct ScheduledSessionRefreshPort {
        publish_on_take: usize,
        takes: usize,
        update: Option<SessionCommandResult>,
    }

    impl SessionRefreshPort for ScheduledSessionRefreshPort {
        fn take(&mut self) -> Option<Result<SessionCommandResult, String>> {
            self.takes += 1;
            (self.takes == self.publish_on_take)
                .then(|| self.update.take().map(Ok))
                .flatten()
        }
    }

    /// A decision lane that counts what the frame loop asked of it. The daemon
    /// round trip belongs to the resident worker, so `refresh` here does exactly
    /// what the production port does: record a wake and return (#551).
    #[derive(Default)]
    struct CountingDecisionPort {
        wakes: Arc<AtomicUsize>,
        polls: Arc<AtomicUsize>,
    }

    impl BackendDecisionPort for CountingDecisionPort {
        fn poll(&mut self, _completions: &Completions) {
            self.polls.fetch_add(1, Ordering::SeqCst);
        }

        fn refresh(&mut self, _workspace: WorkspaceId, _completions: Completions) {
            self.wakes.fetch_add(1, Ordering::SeqCst);
        }

        fn resolve(
            &mut self,
            _workspace: WorkspaceId,
            _decision_id: UserDecisionId,
            _answer: UserDecisionAnswer,
            _completions: Completions,
        ) {
        }
    }

    #[test]
    fn app_event_from_key_maps_ordinary_management_keys() {
        assert_eq!(app_event_from_key(Key::Up), Some(AppEvent::Key(AppKey::Up)));
        assert_eq!(
            app_event_from_key(Key::Down),
            Some(AppEvent::Key(AppKey::Down))
        );
        assert_eq!(
            app_event_from_key(Key::Enter),
            Some(AppEvent::Key(AppKey::Enter))
        );
        assert_eq!(
            app_event_from_key(Key::Backspace),
            Some(AppEvent::Key(AppKey::Backspace))
        );
        assert_eq!(
            app_event_from_key(Key::Paste("貼り付け".to_owned())),
            Some(AppEvent::Key(AppKey::Paste("貼り付け".to_owned())))
        );
        assert_eq!(
            app_event_from_key(Key::Tab),
            Some(AppEvent::Key(AppKey::Tab))
        );
        assert_eq!(
            app_event_from_key(Key::Escape),
            Some(AppEvent::Key(AppKey::Escape))
        );
        assert_eq!(
            app_event_from_key(Key::Char('x')),
            Some(AppEvent::Key(AppKey::Char('x')))
        );
        assert_eq!(
            app_event_from_key(Key::Char('\u{1}')),
            Some(AppEvent::Key(AppKey::CtrlA))
        );
        assert_eq!(
            app_event_from_key(Key::Quit),
            Some(AppEvent::Key(AppKey::CtrlC))
        );
        assert_eq!(
            app_event_from_key(Key::CtrlQ),
            Some(AppEvent::Key(AppKey::CtrlQ))
        );
        assert_eq!(
            app_event_from_key(Key::Management {
                action: AppKey::SaveRoles,
                passthrough: vec![0x13],
            }),
            Some(AppEvent::Key(AppKey::SaveRoles))
        );
    }

    #[test]
    fn app_event_from_key_maps_resolved_live_actions_to_reducer_keys() {
        assert_eq!(
            app_event_from_key(Key::Live(LiveTerminalAction::Switch)),
            Some(AppEvent::Key(AppKey::CtrlO))
        );
        assert_eq!(
            app_event_from_key(Key::Live(LiveTerminalAction::OpenCloseupModal)),
            Some(AppEvent::Key(AppKey::OpenCloseupOverlay))
        );
        assert_eq!(
            app_event_from_key(Key::Live(LiveTerminalAction::NextTab)),
            Some(AppEvent::Key(AppKey::CtrlN))
        );
        assert_eq!(
            app_event_from_key(Key::Live(LiveTerminalAction::PreviousTab)),
            Some(AppEvent::Key(AppKey::CtrlP))
        );
        assert_eq!(
            app_event_from_key(Key::Live(LiveTerminalAction::OpenPullRequests)),
            Some(AppEvent::Key(AppKey::OpenPrs))
        );
        assert_eq!(
            app_event_from_key(Key::Live(LiveTerminalAction::Agent)),
            Some(AppEvent::Key(AppKey::CtrlA))
        );
        assert_eq!(
            app_event_from_key(Key::Live(LiveTerminalAction::Director)),
            Some(AppEvent::Key(AppKey::ToggleDirectorDrawer))
        );
        assert_eq!(
            app_event_from_key(Key::Live(LiveTerminalAction::DirectorNew)),
            Some(AppEvent::Key(AppKey::OpenDirectorNew))
        );
        assert_eq!(
            app_event_from_key(Key::Live(LiveTerminalAction::QuitConfirmation)),
            Some(AppEvent::Key(AppKey::OpenQuitConfirmation))
        );
    }

    #[test]
    fn closeup_live_pr_action_opens_the_active_sessions_modal() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let mut state =
            crate::usecase::application::controller::AppState::home(workspace, vec![session]);
        let _ = crate::usecase::application::controller::update(
            &mut state,
            AppEvent::PaneTabAvailability {
                available: true,
                error: None,
            },
        );
        let _ = crate::usecase::application::controller::update(
            &mut state,
            AppEvent::Key(AppKey::Enter),
        );
        assert_eq!(state.route(), Route::Home(HomeMode::Closeup));
        assert_eq!(state.overlay(), None);

        let event = app_event_from_key(Key::Live(LiveTerminalAction::OpenPullRequests))
            .expect("live PR action maps to a reducer event");
        assert_eq!(
            crate::usecase::application::controller::update(&mut state, event),
            vec![Effect::LoadPullRequests { target }]
        );
        assert_eq!(state.overlay(), Some(Overlay::Prs));
        assert_eq!(state.pr_overlay().unwrap().target(), target);
    }

    #[test]
    fn app_event_from_key_ticks_on_wakeups_and_drops_pane_only_input() {
        // Resize / backend wakeups reach the loop as `Other` and advance the mascot.
        assert_eq!(app_event_from_key(Key::Other), Some(AppEvent::Tick));
        // Raw passthrough and terminal pointer drags never reach the Home reducer.
        assert_eq!(app_event_from_key(Key::Passthrough(vec![0x1b])), None);
        // Sidebar clicks need the real runtime's injected monotonic timestamp.
        assert_eq!(app_event_from_key(Key::Click { column: 3, row: 4 }), None);
        // Left/Right reach the reducer to move the Yes/No confirmation focus; the
        // reducer ignores them outside that overlay. Ctrl-D stays Open-only.
        assert_eq!(
            app_event_from_key(Key::Left),
            Some(AppEvent::Key(AppKey::Left))
        );
        assert_eq!(
            app_event_from_key(Key::Right),
            Some(AppEvent::Key(AppKey::Right))
        );
        assert_eq!(app_event_from_key(Key::CtrlD), None);
        // Tab close and terminal scroll/copy stay pane- and shell-level concerns.
        for action in [
            LiveTerminalAction::CloseTab,
            LiveTerminalAction::ScrollUp,
            LiveTerminalAction::ScrollDown,
            LiveTerminalAction::ScrollBottom,
        ] {
            assert_eq!(app_event_from_key(Key::Live(action)), None);
        }
        let terminal_copy_event = app_event_from_key(Key::TerminalCopy { fallback: vec![3] });
        #[cfg(target_os = "windows")]
        assert_eq!(terminal_copy_event, Some(AppEvent::Key(AppKey::CtrlC)));
        #[cfg(not(target_os = "windows"))]
        assert_eq!(terminal_copy_event, None);
    }

    /// A resize is a redraw, never an inventory refresh. It reaches the reducer
    /// as the same mascot tick as a wake-up, while the real dimensions come from
    /// `term.size()` at the head of the frame; the daemon lanes are not involved
    /// at all (#551).
    #[test]
    fn a_resize_maps_to_a_redraw_tick_and_stays_distinct_from_a_wakeup() {
        assert_eq!(app_event_from_key(Key::Resize), Some(AppEvent::Tick));
        assert_eq!(app_event_from_key(Key::Other), Some(AppEvent::Tick));
        assert_ne!(Key::Resize, Key::Other);
    }

    #[test]
    fn sidebar_pointer_adapter_preserves_coordinates_and_injected_time() {
        let at = std::time::Duration::from_millis(1_234);
        assert_eq!(
            sidebar_pointer_event(3, 4, at),
            AppEvent::Pointer {
                column: 3,
                row: 4,
                at,
            }
        );
    }

    /// The screen saver's deadline must survive an Agent working all night and
    /// end the moment a person touches the terminal, so the classification of
    /// "was that a user?" is pinned over the whole key vocabulary.
    #[test]
    fn only_a_real_interaction_postpones_the_screen_saver() {
        let interactions = [
            Key::Up,
            Key::Down,
            Key::Left,
            Key::Right,
            Key::Home,
            Key::End,
            Key::Delete,
            Key::LineStart,
            Key::LineEnd,
            Key::SelectLeft,
            Key::SelectRight,
            Key::SelectHome,
            Key::SelectEnd,
            Key::Enter,
            Key::Backspace,
            Key::Tab,
            Key::Escape,
            Key::Quit,
            Key::CtrlQ,
            Key::CtrlD,
            Key::Char('g'),
            Key::Paste("pasted".to_owned()),
            Key::Click { column: 4, row: 9 },
            Key::Pointer(PointerEvent {
                kind: PointerKind::Down,
                column: 4,
                row: 9,
            }),
            Key::TerminalCopy { fallback: vec![3] },
            Key::Passthrough(vec![b'x']),
            Key::Live(LiveTerminalAction::ScrollUp),
            Key::Management {
                action: AppKey::Escape,
                passthrough: vec![0x1b],
            },
            // A resize is the user dragging a window edge, and the design has it
            // both close the garden and restart the timer.
            Key::Resize,
        ];
        for key in interactions {
            assert!(is_user_activity(&key), "{key:?} should postpone the garden");
        }
        // The one wake-up that is not a person: frame ticks, drained daemon
        // events, and Agent output all arrive as `Other`.
        assert!(!is_user_activity(&Key::Other));
    }

    /// The watch is a pure elapsed-time fold: the shell reduces its monotonic
    /// clock to a `Duration`, so nothing below it reads a clock at all.
    #[test]
    fn the_idle_watch_measures_from_the_last_interaction() {
        let ms = std::time::Duration::from_millis;
        let mut watch = IdleWatch::new(ms(1_000));

        assert_eq!(watch.observe(&Key::Other, ms(1_500)), ms(500));
        assert_eq!(watch.observe(&Key::Other, ms(9_000)), ms(8_000));
        // An interaction restarts the measurement in the same call.
        assert_eq!(watch.observe(&Key::Char('a'), ms(9_000)), ms(0));
        assert_eq!(watch.observe(&Key::Other, ms(9_400)), ms(400));
        // A clock that appears to run backwards (it cannot, but the arithmetic
        // must not panic if it ever did) reads as "no idle time".
        assert_eq!(watch.observe(&Key::Other, ms(0)), ms(0));
    }

    /// The whole automatic path, minus the single thing that is genuinely the
    /// OS's — the monotonic clock the shell injects. Frame wake-ups accumulate
    /// idle time, an interaction throws it away, the threshold opens the
    /// overlay, the very next frame *is* the garden, and a click resolved
    /// against that frame's own plots lands in the rabbit's Closeup.
    #[test]
    fn an_idle_home_grows_a_garden_whose_usagi_is_one_click_from_its_closeup() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let record = SessionRecord {
            name: "alpha".to_owned(),
            display_name: None,
            origin: SessionOrigin::Human,
            started_from: None,
            root: PathBuf::from("/tmp/demo/alpha"),
            created_at: now(),
            last_active: None,
            notes: Scratchpad::default(),
            prs: Vec::new(),
        };
        let sessions = vec![ProjectedSession::from_record(session, &record)];
        let root = PathBuf::from("/tmp/demo");
        let no_diffs = BTreeMap::new();
        let clock = now();
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let material = |runtime: &WorkspaceRuntime| {
            home_frame_material(
                24,
                80,
                runtime,
                "demo",
                &root,
                &sessions,
                None,
                health(),
                &no_diffs,
                None,
                None,
                clock,
            )
        };

        // The composition root's frame period. Idle time is measured in these,
        // and driving them one by one is what proves the threshold is reached
        // rather than assumed.
        let frame = std::time::Duration::from_millis(16);
        let mut watch = IdleWatch::new(std::time::Duration::ZERO);
        let mut elapsed = std::time::Duration::ZERO;
        let wake = |runtime: &mut WorkspaceRuntime,
                    watch: &mut IdleWatch,
                    elapsed: &mut std::time::Duration,
                    key: &Key| {
            *elapsed += frame;
            let idle = watch.observe(key, *elapsed);
            let _ = runtime.apply_event(AppEvent::IdleElapsed(idle));
        };

        // Four minutes of ticks — an Agent streaming output the whole time —
        // and Home is still Home.
        let four_minutes = GARDEN_IDLE_THRESHOLD
            .checked_sub(std::time::Duration::from_secs(60))
            .expect("the threshold is longer than a minute");
        while elapsed < four_minutes {
            wake(&mut runtime, &mut watch, &mut elapsed, &Key::Other);
        }
        assert_eq!(runtime.state().overlay(), None);

        // One keypress and the four minutes are gone.
        wake(&mut runtime, &mut watch, &mut elapsed, &Key::Down);
        let restarted_at = elapsed;
        while elapsed < restarted_at + GARDEN_IDLE_THRESHOLD {
            assert_eq!(
                runtime.state().overlay(),
                None,
                "the garden opened {:?} after the last interaction",
                elapsed.saturating_sub(restarted_at)
            );
            wake(&mut runtime, &mut watch, &mut elapsed, &Key::Other);
        }
        assert_eq!(runtime.state().overlay(), Some(Overlay::Garden));

        // The next frame is the garden itself, drawn full width over Home.
        let garden = material(&runtime);
        let rows = render_home_material(&garden);
        let text = rows.join("\n");
        assert_eq!(rows.len(), 24);
        assert!(text.contains("Garden · click a usagi to visit · any key to return"));
        assert!(text.contains("alpha"));

        // Every cell of the frame resolves through the same layout that drew it,
        // so the rabbit is reachable by clicking where it is drawn and the rest
        // of the garden is a wake-up.
        let resolve = |column: u16, row: u16| {
            garden_click_at(
                garden.height,
                garden.width,
                &garden.projection,
                garden.now,
                column,
                row,
            )
        };
        // Agent の居ない session なので、押せるのは区画（agent 無しの訪問）である。
        let visit = Some(GardenClick::Visit {
            session,
            agent: None,
        });
        let plot = (0..24)
            .flat_map(|row| (0..80).map(move |column| (column, row)))
            .find(|&(column, row)| resolve(column, row) == visit)
            .expect("the garden draws a clickable usagi for its session");
        assert_eq!(resolve(0, 23), Some(GardenClick::Dismiss));

        let _ = runtime.apply_event(AppEvent::GardenClick(
            resolve(plot.0, plot.1).expect("the click landed on the garden"),
        ));
        assert_eq!(runtime.state().active(), Some(session));
        assert!(matches!(
            runtime.state().route(),
            Route::Home(HomeMode::Closeup)
        ));
        // Home is back: the frame after the visit is no longer the garden.
        let after = render_home_material(&material(&runtime)).join("\n");
        assert!(!after.contains("any key to return"));
    }

    /// Any other press is the documented wake-up: it is consumed, and the Home
    /// from before the screen saver comes back with no target changed.
    #[test]
    fn a_click_beside_the_usagi_only_wakes_the_garden() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
        assert_eq!(runtime.state().overlay(), Some(Overlay::Garden));

        let before = runtime.state().active();
        let _ = runtime.apply_event(AppEvent::GardenClick(GardenClick::Dismiss));
        assert_eq!(runtime.state().overlay(), None);
        assert_eq!(runtime.state().active(), before);
    }

    /// うさぎの click は session を訪問したうえで、その agent 自身の tab を開く。
    /// 区画（nameplate や余白）の click は従来どおり session の Closeup までで、
    /// tab 選択を動かさない。
    #[test]
    fn clicking_a_usagi_opens_that_agents_tab() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let first = TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: workspace,
            session_id: Some(session),
            worktree_id: WorktreeId::new(),
        };
        let second = TerminalRef {
            terminal_id: TerminalId::new(),
            ..first.clone()
        };
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let (interaction, revision) = runtime.restore_fence();
        assert!(runtime.restore_snapshot(
            interaction,
            revision,
            vec![PaneRestoreTarget {
                target: Target::Session(session),
                panes: vec![
                    LivePane {
                        terminal: first.clone(),
                        kind: PaneKind::Agent,
                    },
                    LivePane {
                        terminal: second.clone(),
                        kind: PaneKind::Agent,
                    },
                ],
                selected: Some(first.clone()),
                selected_interrupted: None,
                interrupted: Vec::new(),
            }],
        ));
        let runtime_id = AgentRuntimeId::new();
        let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::RuntimePhase {
            runtime: AgentRuntimeRef {
                agent_runtime_id: runtime_id,
                terminal: second.clone(),
                session_id: Some(session),
            },
            phase: usagi_core::domain::session_lifecycle::AgentPhase::Running,
        }));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));

        // 区画の click（agent 無し）は tab を動かさない。
        let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
        let plot_click = GardenClick::Visit {
            session,
            agent: None,
        };
        let _ = runtime.apply_event(AppEvent::GardenClick(plot_click));
        visit_garden_agent(&mut ui, &mut runtime, plot_click);
        assert_eq!(runtime.focused_terminal(), Some(first.clone()));

        // うさぎの click は、その runtime を持つ tab を選ぶ。
        let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
        let rabbit_click = GardenClick::Visit {
            session,
            agent: Some(runtime_id),
        };
        let _ = runtime.apply_event(AppEvent::GardenClick(rabbit_click));
        visit_garden_agent(&mut ui, &mut runtime, rabbit_click);
        assert_eq!(runtime.focused_terminal(), Some(second.clone()));

        // 押した瞬間に終了していたうさぎは、無関係な tab を選ばない（選択はそのまま）。
        let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
        let gone = GardenClick::Visit {
            session,
            agent: Some(AgentRuntimeId::new()),
        };
        let _ = runtime.apply_event(AppEvent::GardenClick(gone));
        visit_garden_agent(&mut ui, &mut runtime, gone);
        assert_eq!(runtime.focused_terminal(), Some(second));

        // Dismiss は訪問ですらないので、agent の焦点も動かさない。
        visit_garden_agent(&mut ui, &mut runtime, GardenClick::Dismiss);
    }

    /// tab strip がまだ無い session（Closeup が action launcher を開く）では、
    /// うさぎの click は session の訪問までで止まる。launcher の裏の pane を
    /// 勝手に選ばない。
    #[test]
    fn a_rabbit_click_on_a_tabless_session_stops_at_its_closeup() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));

        let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
        let click = GardenClick::Visit {
            session,
            agent: Some(AgentRuntimeId::new()),
        };
        let _ = runtime.apply_event(AppEvent::GardenClick(click));
        assert_eq!(runtime.state().overlay(), Some(Overlay::Closeup));
        visit_garden_agent(&mut ui, &mut runtime, click);
        assert_eq!(runtime.focused_terminal(), None);
    }

    #[test]
    fn failed_delete_selection_renders_the_force_remove_confirmation() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let record = SessionRecord {
            name: "feature".to_owned(),
            display_name: None,
            origin: SessionOrigin::Human,
            started_from: None,
            root: PathBuf::from("/tmp/demo/feature"),
            created_at: now(),
            last_active: None,
            notes: Scratchpad::default(),
            prs: Vec::new(),
        };
        let mut projected = ProjectedSession::from_record(session, &record);
        projected.lifecycle = SessionLifecycle::Failed;
        projected.failure_stage = Some(usagi_core::domain::session_lifecycle::FailureStage::Delete);
        projected.failure_summary = Some("safe detail".to_owned());
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::SessionLifecycles(
            BTreeMap::from([(
                session,
                SessionLifecycleProjection {
                    lifecycle: SessionLifecycle::Failed,
                    failure_stage: Some(
                        usagi_core::domain::session_lifecycle::FailureStage::Delete,
                    ),
                    failure_summary: Some("safe detail".to_owned()),
                },
            )]),
        )));
        let _ = runtime.apply_event(AppEvent::Key(AppKey::Enter));

        let material = home_frame_material(
            20,
            80,
            &runtime,
            "demo",
            Path::new("/tmp/demo"),
            &[projected],
            None,
            health(),
            &BTreeMap::new(),
            None,
            None,
            now(),
        );
        let frame = render_home_material(&material).join("\n");

        assert!(frame.contains("Force remove"));
        assert!(frame.contains("Force remove feature?"));
        assert!(frame.contains("[ yes ]"));
        assert!(frame.contains("[ no  ]"));
        assert!(frame.contains("Previous removal failed. Changes may be discarded."));
    }

    fn ws(name: &str) -> Workspace {
        Workspace::new(name, format!("/tmp/{name}"))
    }

    fn ws_minutes_ago(name: &str, minutes: i64) -> Workspace {
        let mut workspace = ws(name);
        workspace.updated_at = now() - Duration::minutes(minutes);
        workspace
    }

    fn state(name: &str) -> WorkspaceState {
        WorkspaceState {
            sessions: vec![SessionRecord {
                name: format!("{name}-session"),
                display_name: None,
                origin: SessionOrigin::Human,
                started_from: None,
                root: PathBuf::from(format!("/tmp/{name}/session")),
                created_at: now(),
                last_active: None,
                notes: Scratchpad::default(),
                prs: Vec::new(),
            }],
            root_notes: Scratchpad::default(),
            updated_at: now(),
        }
    }

    fn snapshot(name: &str) -> WorkspaceSnapshot {
        WorkspaceSnapshot::new(ws(name), state(name))
    }

    #[test]
    fn session_worktree_names_include_stale_directories_only() {
        let temp = tempdir().unwrap();
        let sessions = temp.path().join(".usagi/sessions");
        std::fs::create_dir_all(sessions.join("stale-session")).unwrap();
        std::fs::write(sessions.join("not-a-worktree"), "marker").unwrap();

        assert_eq!(
            FsSessionWorktreeScanPort.scan(temp.path()),
            vec!["stale-session"]
        );
    }

    #[test]
    fn backend_host_and_explicit_error_adapters_cover_the_full_route_matrix() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let (host, actions) = ControllerHost::channel();
        let mut backend = DaemonBackend::new(
            Box::new(host.clone()),
            Box::new(host),
            Box::new(UnavailableBackendPort),
            Box::new(UnavailableBackendPort),
        )
        .with_decisions(Box::new(UnavailableBackendPort))
        .with_overlay(Box::new(UnavailableBackendPort));

        for effect in [
            Effect::CreateSession {
                workspace,
                token: PendingToken::from_raw(1),
                operation_id: OperationId::new(),
                intent: SessionCreateIntent {
                    name: "feature".to_owned(),
                    profile: None,
                    model: None,
                    role_id: None,
                },
            },
            Effect::RefreshSessions { workspace },
            Effect::RemoveSession {
                workspace,
                session,
                force: true,
                force_delete_branch: false,
            },
            Effect::LaunchAgent {
                workspace,
                session: Some(session),
                operation_id: OperationId::new(),
                profile: None,
            },
            Effect::ResumeAgent {
                workspace,
                session,
                operation_id: OperationId::new(),
            },
            Effect::ReopenAgent {
                workspace,
                continuation: AgentContinuationRef::new(),
            },
            Effect::OpenTerminal {
                target,
                operation_id: OperationId::new(),
                arguments: "new".to_owned(),
            },
            Effect::OpenExternalTerminal { target },
            Effect::SelectTab {
                direction: TabDirection::Next,
            },
        ] {
            backend.dispatch(effect);
        }
        assert_eq!(actions.try_iter().count(), 9);

        for effect in [
            Effect::LoadNotes { target },
            Effect::SaveNotes {
                target,
                scratchpad: Scratchpad::default(),
            },
            Effect::LoadEnvironment {
                scope: EnvScope::Workspace,
            },
            Effect::SaveEnvironment {
                scope: EnvScope::Workspace,
                entries: vec![EnvironmentEntry {
                    name: "KEY".to_owned(),
                    value: "value".to_owned(),
                }],
            },
            Effect::WorkspaceCommand {
                workspace,
                command: crate::usecase::overview::Command::Issue {
                    arguments: "list".to_owned(),
                },
            },
            Effect::RefreshDecisions { workspace },
            Effect::ResolveDecision {
                workspace,
                decision_id: UserDecisionId::new(),
                answer: UserDecisionAnswer::Freeform {
                    text: "answer".to_owned(),
                },
            },
            Effect::LoadPullRequests { target },
            Effect::LoadPreview { target },
            Effect::OpenPullRequest {
                url: "https://github.com/o/r/pull/1".to_owned(),
            },
        ] {
            backend.dispatch(effect);
        }
        assert_eq!(backend.drain_events().len(), 10);
    }

    #[test]
    fn default_agent_port_rejects_legacy_inventory_and_exact_resume() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut port = SuccessfulAgentPort(live_terminal_ref(workspace, session));
        assert_eq!(
            port.resume(workspace, session, OperationId::new())
                .unwrap_err(),
            "Agent resume is unavailable."
        );
        assert_eq!(
            port.resume_inventory(workspace).unwrap_err(),
            "Agent resume inventory is unavailable."
        );
        let target = usagi_core::domain::agent::AgentResumeTarget {
            continuation: usagi_core::domain::id::AgentContinuationRef::new(),
            source: usagi_core::domain::id::AgentResumeSourceId::new(),
            workspace_id: workspace,
            session_id: Some(session),
            worktree_id: WorktreeId::new(),
            runtime_id: usagi_core::domain::id::AgentRuntimeId::new(),
            adapter_revision: 1,
        };
        assert_eq!(
            port.resume_exact(target, OperationId::new()).unwrap_err(),
            "Exact Agent resume is unavailable."
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One host fixture verifies the ordered action-routing contract.
    fn controller_host_executor_routes_busy_launch_terminal_and_tab_actions() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let mut pending = std::collections::HashMap::new();
        let (host, actions) = ControllerHost::channel();
        let mut backend = DaemonBackend::new(
            Box::new(host.clone()),
            Box::new(host),
            Box::new(UnavailableBackendPort),
            Box::new(UnavailableBackendPort),
        );
        let token = PendingToken::from_raw(90);

        for effect in [
            Effect::CreateSession {
                workspace,
                token,
                operation_id: OperationId::new(),
                intent: SessionCreateIntent {
                    name: "feature".into(),
                    profile: None,
                    model: None,
                    role_id: None,
                },
            },
            Effect::RefreshSessions { workspace },
            Effect::RemoveSession {
                workspace,
                session: SessionId::new(),
                force: false,
                force_delete_branch: false,
            },
            Effect::LaunchAgent {
                workspace,
                session: Some(session),
                operation_id: OperationId::new(),
                profile: None,
            },
            Effect::ResumeAgent {
                workspace,
                session,
                operation_id: OperationId::new(),
            },
            Effect::ReopenAgent {
                workspace: WorkspaceId::new(),
                continuation: AgentContinuationRef::new(),
            },
            Effect::OpenTerminal {
                target,
                operation_id: OperationId::new(),
                arguments: "new".into(),
            },
            Effect::OpenExternalTerminal { target },
            Effect::OpenExternalTerminal {
                target: Target::Session(SessionId::new()),
            },
            Effect::SelectTab {
                direction: TabDirection::Previous,
            },
        ] {
            backend.dispatch(effect);
        }
        drain_host_actions(&actions, &mut ui, &mut runtime, &mut pending);
        let completed = (0..1)
            .map(|_| {
                ui.session_completions
                    .recv_timeout(std::time::Duration::from_secs(1))
                    .expect("session command completion")
            })
            .collect::<Vec<_>>();
        for completion in completed {
            ui.session_completion_sender.send(completion).unwrap();
        }
        super::drain_session_completions(&mut ui);
        let events = backend.drain_events();
        // Create is admitted and reports its token; Remove is refused as busy
        // and notices. `RefreshSessions` no longer competes for that single
        // command slot at all — it parks on the resident lane, which observes
        // nothing here, so it contributes no event (#551).
        assert_eq!(events.len(), 2);
        assert!(events.iter().any(|event| matches!(
            event,
            AppEvent::OperationResult(result) if result.token == token && !result.succeeded
        )));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AppEvent::Backend(BackendEvent::Notice(_))))
                .count(),
            1
        );
        assert_eq!(ui.pane_launches.len(), 2);
        assert!(!pending.is_empty());

        let calls = Arc::new(Mutex::new(Vec::new()));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(SnapshotSessionPort(calls.clone())))
            .with_agent_context(
                workspace,
                vec![session],
                Box::new(SuccessfulAgentPort(live_terminal_ref(workspace, session))),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let mut pending = std::collections::HashMap::new();
        let (host, actions) = ControllerHost::channel();
        let mut backend = DaemonBackend::new(
            Box::new(host.clone()),
            Box::new(host),
            Box::new(UnavailableBackendPort),
            Box::new(UnavailableBackendPort),
        );
        backend.dispatch(Effect::RefreshSessions { workspace });
        drain_host_actions(&actions, &mut ui, &mut runtime, &mut pending);
        std::thread::sleep(std::time::Duration::from_millis(10));
        super::drain_session_completions(&mut ui);
        backend.dispatch(Effect::RemoveSession {
            workspace,
            session,
            force: true,
            force_delete_branch: false,
        });
        drain_host_actions(&actions, &mut ui, &mut runtime, &mut pending);
        std::thread::sleep(std::time::Duration::from_millis(10));
        super::drain_session_completions(&mut ui);
        backend.dispatch(Effect::OpenTerminal {
            target,
            operation_id: OperationId::new(),
            arguments: "new".into(),
        });
        drain_host_actions(&actions, &mut ui, &mut runtime, &mut pending);
        // Only the user-initiated `Remove` reaches the command port. The
        // refresh went to the resident lane, so it neither spawned a worker nor
        // opened a connection of its own (#551).
        assert_eq!(
            calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );
    }

    #[test]
    fn right_pane_click_selection_reaches_the_runtime_tab_owner() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let _ = runtime.apply_event(AppEvent::Key(AppKey::Enter));
        let first_operation = OperationId::new();
        let first = live_terminal_ref(workspace, session);
        let _ = runtime.request_pane(target, first_operation, PaneKind::Terminal);
        let _ = runtime.complete_pane(target, first_operation, first.clone());
        let second_operation = OperationId::new();
        let second = live_terminal_ref(workspace, session);
        let _ = runtime.request_pane(target, second_operation, PaneKind::Terminal);
        let _ = runtime.complete_pane(target, second_operation, second);

        // A stale/out-of-range frame hit is inert.
        select_right_pane_tab(&mut ui, &mut runtime, usize::MAX);
        select_right_pane_tab(&mut ui, &mut runtime, 0);

        assert_eq!(runtime.focused_terminal(), Some(first));
    }

    #[test]
    fn right_pane_agent_click_commits_intent_before_selection_and_surfaces_failure() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let first = live_terminal_ref(workspace, session);
        let second = live_terminal_ref(workspace, session);
        let continuation = AgentContinuationRef::new();
        let interrupted = interrupted_history(workspace, Some(session), true);
        let mut intent = AgentTabIntent::empty(workspace);
        intent.apply(AgentTabIntentMutation::Upsert {
            session_id: Some(session),
            continuation,
            terminal: first.clone(),
            select: true,
        });
        intent.apply(AgentTabIntentMutation::Upsert {
            session_id: Some(session),
            continuation: interrupted.continuation,
            terminal: interrupted.last_terminal.clone(),
            select: false,
        });
        let durable = Arc::new(Mutex::new(intent.clone()));
        let mutations = Arc::new(Mutex::new(Vec::new()));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(
                workspace,
                vec![session],
                Box::new(UnavailableAgentCommandPort),
            )
            .with_agent_tab_intent(
                workspace,
                BTreeSet::from([session]),
                Box::new(MemoryIntentPort {
                    state: Arc::clone(&durable),
                    mutations: Arc::clone(&mutations),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let _ = runtime.apply_event(AppEvent::Key(AppKey::Enter));
        for terminal in [first.clone(), second.clone()] {
            let operation = OperationId::new();
            let _ = runtime.request_pane(target, operation, PaneKind::Agent);
            let _ = runtime.complete_pane(target, operation, terminal);
        }
        runtime.inject_pane_event_for_test(
            target,
            crate::usecase::application::pane::PaneEvent::RestoreInterrupted {
                tabs: vec![interrupted.clone()],
            },
        );
        let pending = OperationId::new();
        let _ = runtime.request_pane(target, pending, PaneKind::Agent);

        let interrupted_index = runtime
            .active_pane()
            .tabs()
            .iter()
            .position(|tab| {
                matches!(tab, PaneTab::Interrupted(pane) if pane.tab.continuation == interrupted.continuation)
            })
            .expect("interrupted tab is visible");
        let pending_index = runtime
            .active_pane()
            .tabs()
            .iter()
            .position(|tab| matches!(tab, PaneTab::Pending(pane) if pane.operation == pending))
            .expect("pending tab is visible");

        select_right_pane_tab(&mut ui, &mut runtime, pending_index);
        select_right_pane_tab(&mut ui, &mut runtime, interrupted_index);

        select_right_pane_tab(&mut ui, &mut runtime, 1);

        assert_eq!(runtime.focused_terminal(), Some(second.clone()));
        assert!(mutations.lock().unwrap().iter().any(|mutation| matches!(
            mutation,
            AgentTabIntentMutation::Select {
                session_id: Some(selected),
                continuation: None,
            } if *selected == session
        )));

        let attempts = Arc::new(AtomicUsize::new(0));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut failing_ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(
                workspace,
                vec![session],
                Box::new(UnavailableAgentCommandPort),
            )
            .with_agent_tab_intent(
                workspace,
                BTreeSet::from([session]),
                Box::new(FailingIntentPort {
                    state: Arc::new(Mutex::new(intent)),
                    error: AgentTabIntentError::Unavailable,
                    attempts: Arc::clone(&attempts),
                }),
            );
        select_right_pane_tab(&mut failing_ui, &mut runtime, 0);

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.focused_terminal(), Some(second));
    }

    #[test]
    fn a_failed_lifecycle_flows_to_the_sidebar_rows_and_the_reducer() {
        use usagi_core::domain::session_lifecycle::{SessionLifecycle, SessionLifecycleProjection};
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        view.set_session_lifecycles(std::collections::BTreeMap::from([(
            session,
            SessionLifecycleProjection {
                lifecycle: SessionLifecycle::Failed,
                failure_stage: Some(usagi_core::domain::session_lifecycle::FailureStage::Create),
                failure_summary: Some("create failed".into()),
            },
        )]));
        let role = crate::usecase::application::controller::SessionRoleProjection {
            role_id: None,
            role_summary: Some("Reviewer".into()),
            parent_session_id: None,
            agent_status: None,
        };
        view.set_session_roles(BTreeMap::from([(session, role.clone())]));
        let ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));

        // The projected sidebar row carries the Failed lifecycle and its reason.
        let mut state =
            crate::usecase::application::controller::AppState::home(workspace, vec![session]);
        let _ = crate::usecase::application::controller::update(
            &mut state,
            crate::usecase::application::controller::AppEvent::Backend(
                crate::usecase::application::controller::BackendEvent::PullRequestsLoaded {
                    target: Target::Session(session),
                    revision: 1,
                    prs: vec![usagi_core::domain::pullrequest::PrLink::new(
                        1545,
                        "https://example.test/pull/1545",
                    )],
                },
            ),
        );
        let rows = super::project_controller_sessions(&ui, &state);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].lifecycle, SessionLifecycle::Failed);
        assert_eq!(rows[0].failure_summary.as_deref(), Some("create failed"));
        assert!(!rows[0].removing);
        assert!(rows[0].pr_summary.is_some());

        // The reducer receives the lifecycle so it can gate attach by capability.
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
        super::sync_runtime_sessions(&mut runtime, &ui, &[]);
        assert_eq!(runtime.state().sessions(), &[session]);
        assert_eq!(runtime.state().session_roles().get(&session), Some(&role));
        assert_eq!(
            runtime
                .state()
                .session_lifecycles()
                .get(&session)
                .map(|projection| projection.lifecycle),
            Some(SessionLifecycle::Failed),
        );
    }

    #[test]
    fn director_organization_projects_statuses_hierarchy_and_orphans() {
        use usagi_core::domain::agent::AgentStatus;

        let mut empty_state = state("empty");
        empty_state.sessions.clear();
        let empty_ui = WorkspaceUi::new(
            WorkspaceView::with_runtime_ids(ws("empty"), empty_state, Vec::new()),
            Box::new(UnavailableSessionCommandPort),
        );
        assert!(director_organization(&empty_ui).is_empty());

        let director_child = SessionId::new();
        let manager_child = SessionId::new();
        let stopped_child = SessionId::new();
        let running_child = SessionId::new();
        let failed_child = SessionId::new();
        let orphan = SessionId::new();
        let ids = vec![
            director_child,
            manager_child,
            stopped_child,
            running_child,
            failed_child,
            orphan,
        ];
        let mut workspace_state = state("demo");
        let template = workspace_state.sessions[0].clone();
        workspace_state.sessions = [
            "manager", "worker", "stopped", "running", "failed", "orphan",
        ]
        .into_iter()
        .map(|name| SessionRecord {
            name: name.into(),
            root: PathBuf::from(format!("/tmp/demo/{name}")),
            ..template.clone()
        })
        .collect();
        let mut view = WorkspaceView::with_runtime_ids(ws("demo"), workspace_state, ids.clone());
        let role = |parent_session_id, agent_status| {
            crate::usecase::application::controller::SessionRoleProjection {
                role_id: None,
                role_summary: None,
                parent_session_id,
                agent_status,
            }
        };
        let mut roles = BTreeMap::from([
            (director_child, role(None, Some(AgentStatus::Starting))),
            (
                manager_child,
                role(Some(director_child), Some(AgentStatus::Idle)),
            ),
            (
                stopped_child,
                role(Some(manager_child), Some(AgentStatus::Exited)),
            ),
            (running_child, role(None, Some(AgentStatus::Running))),
            (
                failed_child,
                role(Some(running_child), Some(AgentStatus::Failed)),
            ),
            // A corrupt self-cycle is emitted as a root-level orphan and must
            // not increase projection depth or loop forever.
            (orphan, role(Some(orphan), None)),
        ]);
        roles.get_mut(&director_child).unwrap().role_id =
            Some(usagi_core::domain::role::RoleId::new("manager").expect("valid company role"));
        view.set_session_roles(roles);
        let ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));

        let rows = director_organization(&ui);
        assert_eq!(
            rows.iter()
                .map(|row| (row.depth, row.label.as_str(), row.status.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (0, "♛ Director", "active"),
                (1, "◆ Manager · manager", "starting"),
                (2, "• Executor · worker", "waiting"),
                (3, "• Executor · stopped", "stopped"),
                (1, "• Executor · running", "running"),
                (2, "• Executor · failed", "failed"),
                (1, "• Executor · orphan", "ready"),
            ]
        );

        let state =
            crate::usecase::application::controller::AppState::home(WorkspaceId::new(), ids);
        let projected = super::project_controller_sessions(&ui, &state);
        assert_eq!(
            projected
                .iter()
                .map(|session| (session.label.as_str(), session.organization_depth))
                .collect::<Vec<_>>(),
            vec![
                ("manager", 1),
                ("worker", 2),
                ("stopped", 3),
                ("running", 1),
                ("failed", 2),
                ("orphan", 1),
            ]
        );
        assert_eq!(projected[1].parent_session_id, Some(director_child));
    }

    #[test]
    fn a_deleting_lifecycle_keeps_the_row_marked_removing_without_a_local_command() {
        use usagi_core::domain::session_lifecycle::{SessionLifecycle, SessionLifecycleProjection};
        let session = SessionId::new();
        let mut view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        view.set_session_lifecycles(std::collections::BTreeMap::from([(
            session,
            SessionLifecycleProjection {
                lifecycle: SessionLifecycle::Deleting,
                failure_stage: None,
                failure_summary: None,
            },
        )]));
        let ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));

        // The daemon accepts a removal before its worktree teardown runs, so the
        // row stays marked as being removed on the strength of the daemon's
        // lifecycle alone — this TUI never issued the command.
        let state = crate::usecase::application::controller::AppState::home(
            WorkspaceId::new(),
            vec![session],
        );
        let rows = super::project_controller_sessions(&ui, &state);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].lifecycle, SessionLifecycle::Deleting);
        assert!(rows[0].removing);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One shell fixture keeps port absence and async completion in sequence.
    fn workspace_shell_harness_covers_port_absence_projection_and_async_launch_completion() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let terminal = live_terminal_ref(workspace, session);
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));

        ui.start_terminal_session(terminal.clone(), Geometry { cols: 20, rows: 5 });
        ui.set_allowed_agent_sessions(BTreeSet::new());
        let allowed_sessions = BTreeSet::from([session]);
        ui.set_allowed_agent_sessions(allowed_sessions.iter().copied());
        ui.resize_terminals(Geometry { cols: 20, rows: 5 });
        assert!(ui.send_terminal_bytes(&terminal, b"x").is_err());
        assert!(ui.poll_all_terminals().is_empty());
        assert_eq!(
            super::session_name_for(&ui, session).as_deref(),
            Some("demo-session")
        );
        assert_eq!(super::session_name_for(&ui, SessionId::new()), None);

        let records = ui.workspace.sessions().to_vec();
        super::apply_session_projection(&mut ui, None, None, None, None, None);
        super::apply_session_projection(&mut ui, Some(records.clone()), None, None, None, None);
        super::apply_session_projection(
            &mut ui,
            Some(records),
            Some(vec![session]),
            None,
            None,
            None,
        );
        let records = ui.workspace.sessions().to_vec();
        super::apply_session_projection(
            &mut ui,
            Some(records),
            Some(vec![session]),
            Some(std::collections::BTreeMap::new()),
            Some(std::collections::BTreeMap::new()),
            None,
        );
        let mut mismatched_runtime = WorkspaceRuntime::new(workspace, Vec::new());
        ui.pane_completion_sender
            .send(super::PaneLaunchCompletion {
                launch_id: super::PANE_LAUNCH_UNADMITTED,
                outcome: super::PaneLaunchOutcome::Terminal {
                    operation: OperationId::new(),
                    result: Err("late completion without an Agent port".to_owned()),
                },
            })
            .unwrap();
        super::drain_pane_completions_into_runtime(
            &mut ui,
            &mut mismatched_runtime,
            &mut std::collections::HashMap::new(),
            Geometry { cols: 20, rows: 5 },
        );
        super::sync_runtime_sessions(&mut mismatched_runtime, &ui, &[]);
        let mut no_controls = LiveTerminalControls::default();
        let _ = super::poll_and_project_terminals(
            &mut ui,
            &mut mismatched_runtime,
            &mut no_controls,
            Geometry { cols: 20, rows: 5 },
        );
        let mut ui = ui
            .with_agent_context(
                workspace,
                vec![session],
                Box::new(SuccessfulAgentPort(terminal.clone())),
            )
            .with_pane_launch_port(launch_port(Box::new(SuccessfulAgentPort(terminal.clone()))));
        assert!(ui.send_terminal_bytes(&terminal, b"missing").is_err());
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        for session in [Some(session), None] {
            ui.pane_launches.push(super::PaneLaunch::Agent {
                operation: OperationId::new(),
                workspace,
                session,
                profile: None,
                resume: true,
            });
            super::drain_pane_launches(&mut ui, Geometry { cols: 20, rows: 5 });
            std::thread::sleep(std::time::Duration::from_millis(10));
            super::drain_pane_completions_into_runtime(
                &mut ui,
                &mut runtime,
                &mut std::collections::HashMap::new(),
                Geometry { cols: 20, rows: 5 },
            );
        }
        let operation = OperationId::new();
        runtime.on_effect(&Effect::LaunchAgent {
            workspace,
            session: Some(session),
            operation_id: operation,
            profile: None,
        });
        ui.pane_launches.push(super::PaneLaunch::Agent {
            operation,
            workspace,
            session: Some(session),
            profile: None,
            resume: false,
        });
        let mut pending = std::collections::HashMap::from([(operation, target)]);
        super::drain_pane_launches(&mut ui, Geometry { cols: 20, rows: 5 });
        std::thread::sleep(std::time::Duration::from_millis(10));
        super::drain_pane_completions_into_runtime(
            &mut ui,
            &mut runtime,
            &mut pending,
            Geometry { cols: 20, rows: 5 },
        );
        assert!(pending.is_empty());
        ui.resize_terminals(Geometry { cols: 30, rows: 6 });
        let projected_records = ui.workspace.sessions().to_vec();
        super::apply_session_projection(
            &mut ui,
            Some(projected_records),
            Some(vec![session]),
            None,
            Some(std::collections::BTreeMap::from([(
                session,
                usagi_core::domain::session_lifecycle::SessionLifecycleProjection {
                    lifecycle: usagi_core::domain::session_lifecycle::SessionLifecycle::Available,
                    failure_stage: None,
                    failure_summary: None,
                },
            )])),
            None,
        );

        let operation = OperationId::new();
        runtime.on_effect(&Effect::OpenTerminal {
            target,
            operation_id: operation,
            arguments: "new".into(),
        });
        ui.pane_launches.push(super::PaneLaunch::Terminal {
            operation,
            workspace,
            session: Some(session),
            arguments: "new".into(),
        });
        pending.insert(operation, target);
        super::drain_pane_launches(&mut ui, Geometry { cols: 20, rows: 5 });
        std::thread::sleep(std::time::Duration::from_millis(10));
        super::drain_pane_completions_into_runtime(
            &mut ui,
            &mut runtime,
            &mut pending,
            Geometry { cols: 20, rows: 5 },
        );

        ui.pane_completion_sender
            .send(super::PaneLaunchCompletion {
                launch_id: super::PANE_LAUNCH_UNADMITTED,
                outcome: super::PaneLaunchOutcome::Agent {
                    operation: OperationId::new(),
                    result: Ok(AgentPaneAdmission {
                        terminal: terminal.clone(),
                        continuation: None,
                    }),
                },
            })
            .unwrap();
        super::drain_pane_completions_into_runtime(
            &mut ui,
            &mut runtime,
            &mut pending,
            Geometry { cols: 20, rows: 5 },
        );

        let failed_agent = OperationId::new();
        runtime.on_effect(&Effect::LaunchAgent {
            workspace,
            session: Some(session),
            operation_id: failed_agent,
            profile: None,
        });
        pending.insert(failed_agent, target);
        ui.pane_completion_sender
            .send(super::PaneLaunchCompletion {
                launch_id: super::PANE_LAUNCH_UNADMITTED,
                outcome: super::PaneLaunchOutcome::Agent {
                    operation: failed_agent,
                    result: Err("safe Agent launch failure".to_owned()),
                },
            })
            .unwrap();
        super::drain_pane_completions_into_runtime(
            &mut ui,
            &mut runtime,
            &mut pending,
            Geometry { cols: 20, rows: 5 },
        );
        assert!(pending.is_empty());

        ui.pane_completion_sender
            .send(super::PaneLaunchCompletion {
                launch_id: super::PANE_LAUNCH_UNADMITTED,
                outcome: super::PaneLaunchOutcome::Terminal {
                    operation: OperationId::new(),
                    result: Err("late terminal failure".to_owned()),
                },
            })
            .unwrap();
        super::drain_pane_completions_into_runtime(
            &mut ui,
            &mut runtime,
            &mut pending,
            Geometry { cols: 20, rows: 5 },
        );

        let cancel = OperationId::new();
        runtime.on_effect(&Effect::OpenTerminal {
            target,
            operation_id: cancel,
            arguments: "open".into(),
        });
        ui.pane_launches.push(super::PaneLaunch::Terminal {
            operation: cancel,
            workspace,
            session: Some(session),
            arguments: "open".into(),
        });
        pending.insert(cancel, target);
        let _ = runtime.select_tab(TabDirection::Next);
        super::close_focused_terminal_pane(&mut ui, &mut runtime, &mut pending);

        // Two more requests: admission takes exactly one worker and leaves the
        // rest visibly pending, without ever touching the stream port.
        assert!(ui.poll_all_terminals().is_empty());
        let queued_before = ui.pane_launches.len();
        ui.pane_launches.push(super::PaneLaunch::Agent {
            operation: OperationId::new(),
            workspace,
            session: Some(session),
            profile: None,
            resume: false,
        });
        ui.pane_launches.push(super::PaneLaunch::Terminal {
            operation: OperationId::new(),
            workspace,
            session: Some(session),
            arguments: "open".into(),
        });
        super::drain_pane_launches(&mut ui, Geometry { cols: 20, rows: 5 });
        assert_eq!(ui.pane_launches.len(), queued_before + 1);
        assert!(ui.active_pane_launch.is_some());
        assert!(ui.poll_all_terminals().is_empty());
    }

    /// Every resident-stream call the live panes made, so a test can prove pane IO
    /// continued while a launch worker was stopped inside the daemon client.
    #[derive(Default)]
    struct StreamCalls {
        /// Launches asked of the *stream* port. It must stay 0: launches belong to
        /// the dedicated launch client.
        launches: usize,
        attaches: usize,
        /// The viewport each attach stated: a window claims its share of the
        /// terminal's geometry with the attach itself.
        attach_geometries: Vec<(TerminalRef, Geometry)>,
        polls: usize,
        inputs: Vec<Vec<u8>>,
        resizes: usize,
        resize_geometries: Vec<(TerminalRef, Geometry)>,
        /// The shared viewport this daemon answers a resize with, when it is not
        /// the request (another window holds this terminal smaller).
        effective_geometry: Option<Geometry>,
        detaches: usize,
    }

    struct RecordingStreamPort(Arc<Mutex<StreamCalls>>);

    #[coverage(off)] // coverage: reason=generic_monomorphization owner=tui expires=2027-01-31 tests=a_blocked_pane_launch_keeps_every_live_pane_streaming
    impl AgentCommandPort for RecordingStreamPort {
        fn launch(
            &mut self,
            _operation: OperationId,
            _workspace: WorkspaceId,
            _session: Option<SessionId>,
            _profile: Option<AgentProfileId>,
        ) -> Result<AgentPaneAdmission, String> {
            self.0.lock().unwrap().launches += 1;
            Err("the resident stream port never launches".to_owned())
        }

        fn attach_terminal(
            &mut self,
            terminal: &TerminalRef,
            geometry: Geometry,
        ) -> Result<TerminalAttach, TerminalError> {
            let mut calls = self.0.lock().unwrap();
            calls.attaches += 1;
            calls.attach_geometries.push((terminal.clone(), geometry));
            drop(calls);
            Ok(TerminalAttach {
                subscription: TerminalSubscription { id: 9, epoch: 1 },
                revision: 1,
                output_offset: b"one\r\ntwo\r\nthree".len() as u64,
                next_input_seq: None,
                screen: attach_checkpoint(b"one\r\ntwo\r\nthree", geometry),
                exited: false,
            })
        }

        fn poll_terminal(
            &mut self,
            _terminal: &TerminalRef,
            _after_offset: u64,
        ) -> Result<Vec<TerminalChunk>, TerminalError> {
            self.0.lock().unwrap().polls += 1;
            Ok(Vec::new())
        }

        fn input_terminal(
            &mut self,
            _terminal: &TerminalRef,
            _subscription: TerminalSubscription,
            _input_seq: u64,
            _operation: OperationId,
            bytes: &[u8],
        ) -> Result<TerminalInputOutcome, TerminalError> {
            self.0.lock().unwrap().inputs.push(bytes.to_vec());
            Ok(TerminalInputOutcome::Written)
        }

        fn resize_terminal(
            &mut self,
            terminal: &TerminalRef,
            geometry: Geometry,
        ) -> Result<Geometry, TerminalError> {
            let mut calls = self.0.lock().unwrap();
            calls.resizes += 1;
            calls.resize_geometries.push((terminal.clone(), geometry));
            Ok(calls.effective_geometry.unwrap_or(geometry))
        }

        fn detach_terminal(
            &mut self,
            _terminal: &TerminalRef,
            _subscription: TerminalSubscription,
        ) {
            self.0.lock().unwrap().detaches += 1;
        }
    }

    /// A launch client the test stops inside the request: `entered` announces the
    /// admitted request, `release` lets it answer, and `finished` reports that the
    /// worker left the client. Nothing here sleeps, so the barrier is exact.
    struct GatedLaunchPort {
        terminal: TerminalRef,
        entered: Mutex<std::sync::mpsc::Sender<&'static str>>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
        finished: Mutex<std::sync::mpsc::Sender<&'static str>>,
    }

    impl GatedLaunchPort {
        fn gate(&self, kind: &'static str) {
            let _ = self.entered.lock().unwrap().send(kind);
            let _ = self.release.lock().unwrap().recv();
            let _ = self.finished.lock().unwrap().send(kind);
        }
    }

    #[coverage(off)] // coverage: reason=generic_monomorphization owner=tui expires=2027-01-31 tests=a_blocked_pane_launch_keeps_every_live_pane_streaming
    impl PaneLaunchCommandPort for GatedLaunchPort {
        fn launch(
            &self,
            _operation: OperationId,
            _workspace: WorkspaceId,
            _session: Option<SessionId>,
            _profile: Option<AgentProfileId>,
        ) -> Result<AgentPaneAdmission, String> {
            self.gate("launch");
            Ok(AgentPaneAdmission {
                terminal: self.terminal.clone(),
                continuation: None,
            })
        }

        fn resume(
            &self,
            _workspace: WorkspaceId,
            _session: SessionId,
            _operation: OperationId,
        ) -> Result<AgentPaneAdmission, String> {
            self.gate("resume");
            Err("resume is not scripted".to_owned())
        }

        fn resume_exact(
            &self,
            _target: AgentResumeTarget,
            _operation: OperationId,
        ) -> Result<ExactAgentResume, String> {
            self.gate("resume_exact");
            Err("exact resume is not scripted".to_owned())
        }

        fn launch_terminal(
            &self,
            _workspace: WorkspaceId,
            _session: Option<SessionId>,
            _geometry: Geometry,
            _arguments: &str,
            _operation: OperationId,
        ) -> Result<TerminalRef, String> {
            self.gate("launch_terminal");
            Ok(self.terminal.clone())
        }
    }

    /// A launch client that dies on its first request and answers the next one, so
    /// a test can prove the shared client survives a worker's unwind.
    struct PanickingLaunchPort {
        terminal: TerminalRef,
        calls: Arc<AtomicUsize>,
    }

    #[coverage(off)] // coverage: reason=generic_monomorphization owner=tui expires=2027-01-31 tests=a_panicking_launch_worker_fails_only_its_pane
    impl PaneLaunchCommandPort for PanickingLaunchPort {
        fn launch(
            &self,
            _operation: OperationId,
            _workspace: WorkspaceId,
            _session: Option<SessionId>,
            _profile: Option<AgentProfileId>,
        ) -> Result<AgentPaneAdmission, String> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            assert!(call > 0, "launch client died");
            Ok(AgentPaneAdmission {
                terminal: self.terminal.clone(),
                continuation: None,
            })
        }

        fn resume(
            &self,
            _workspace: WorkspaceId,
            _session: SessionId,
            _operation: OperationId,
        ) -> Result<AgentPaneAdmission, String> {
            Err("resume is not scripted".to_owned())
        }

        fn resume_exact(
            &self,
            _target: AgentResumeTarget,
            _operation: OperationId,
        ) -> Result<ExactAgentResume, String> {
            Err("exact resume is not scripted".to_owned())
        }

        fn launch_terminal(
            &self,
            _workspace: WorkspaceId,
            _session: Option<SessionId>,
            _geometry: Geometry,
            _arguments: &str,
            _operation: OperationId,
        ) -> Result<TerminalRef, String> {
            Err("terminal launch is not scripted".to_owned())
        }
    }

    /// A `WorkspaceUi` whose resident stream records its calls and whose launches
    /// go to a separate, test-controlled client.
    fn ui_with_split_ports(
        workspace: WorkspaceId,
        session: SessionId,
        stream: Arc<Mutex<StreamCalls>>,
        launch: Box<dyn PaneLaunchCommandPort>,
    ) -> WorkspaceUi {
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(
                workspace,
                vec![session],
                Box::new(RecordingStreamPort(stream)),
            )
            .with_pane_launch_port(launch)
    }

    fn agent_launch(
        workspace: WorkspaceId,
        session: SessionId,
        operation: OperationId,
    ) -> super::PaneLaunch {
        super::PaneLaunch::Agent {
            operation,
            workspace,
            session: Some(session),
            profile: None,
            resume: false,
        }
    }

    /// Take exactly `count` completions off the worker channel, then put them back
    /// through the drain the frame loop uses. Nothing sleeps, so the number of
    /// completions each request produced is exact.
    fn drain_completions(
        ui: &mut WorkspaceUi,
        runtime: &mut WorkspaceRuntime,
        pending: &mut std::collections::HashMap<OperationId, Target>,
        count: usize,
    ) -> Vec<super::PaneLaunchOutcome> {
        let taken = (0..count)
            .map(|_| {
                ui.pane_completions
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .expect("every admitted request owes exactly one completion")
            })
            .collect::<Vec<_>>();
        let outcomes = taken
            .iter()
            .map(|completion| completion.outcome.clone())
            .collect();
        for completion in taken {
            ui.pane_completion_sender
                .send(completion)
                .expect("the workspace still owns its completion receiver");
        }
        super::drain_pane_completions_into_runtime(ui, runtime, pending, terminal_geometry(20, 80));
        outcomes
    }

    fn drain_next_completion(
        ui: &mut WorkspaceUi,
        runtime: &mut WorkspaceRuntime,
        pending: &mut std::collections::HashMap<OperationId, Target>,
    ) -> super::PaneLaunchOutcome {
        drain_completions(ui, runtime, pending, 1).remove(0)
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One barrier fixture keeps both panes' IO in sequence.
    fn a_blocked_pane_launch_keeps_every_live_pane_streaming() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let first = live_terminal_ref(workspace, session);
        let second = live_terminal_ref(workspace, session);
        let launched = scoped_terminal_ref(workspace, Some(session));
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished) = std::sync::mpsc::channel();
        let stream = Arc::new(Mutex::new(StreamCalls::default()));
        let mut ui = ui_with_split_ports(
            workspace,
            session,
            Arc::clone(&stream),
            Box::new(GatedLaunchPort {
                terminal: launched.clone(),
                entered: Mutex::new(entered_tx),
                release: Mutex::new(release_rx),
                finished: Mutex::new(finished_tx),
            }),
        );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        ui.start_terminal_session(first.clone(), terminal_geometry(20, 80));
        ui.start_terminal_session(second.clone(), terminal_geometry(20, 80));
        assert_eq!(stream.lock().unwrap().attaches, 2);

        let blocked = OperationId::new();
        let queued = OperationId::new();
        let mut pending = std::collections::HashMap::new();
        for operation in [blocked, queued] {
            runtime.on_effect(&Effect::LaunchAgent {
                workspace,
                session: Some(session),
                operation_id: operation,
                profile: None,
            });
            pending.insert(operation, target);
            super::enqueue_pane_launch(&mut ui, agent_launch(workspace, session, operation));
        }

        // One worker is admitted and stops inside the launch client. A second
        // drain must not start another request on it.
        super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
        assert_eq!(
            entered.recv_timeout(std::time::Duration::from_secs(10)),
            Ok("launch")
        );
        super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
        assert_eq!(ui.pane_launches.len(), 1);
        assert!(ui.active_pane_launch.is_some());
        assert!(entered.try_recv().is_err());

        // The stopped worker owns nothing the live panes need: both keep polling,
        // accepting input, resizing, and detaching.
        let resizes_before = stream.lock().unwrap().resizes;
        ui.resize_terminals(terminal_geometry(24, 100));
        assert!(ui.poll_all_terminals().is_empty());
        assert_eq!(ui.send_terminal_bytes(&first, b"a"), Ok(()));
        assert_eq!(ui.send_terminal_bytes(&second, b"b"), Ok(()));
        ui.close_terminal(&second);
        {
            let observed = stream.lock().unwrap();
            assert_eq!(observed.launches, 0);
            assert_eq!(observed.resizes, resizes_before + 2);
            assert_eq!(observed.polls, 2);
            assert_eq!(observed.inputs, [b"a".to_vec(), b"b".to_vec()]);
            assert_eq!(observed.detaches, 1);
        }

        // Releasing the barrier completes exactly the admitted pane and frees
        // admission for the one that stayed pending.
        release.send(()).unwrap();
        assert_eq!(
            finished.recv_timeout(std::time::Duration::from_secs(10)),
            Ok("launch")
        );
        let outcome = drain_next_completion(&mut ui, &mut runtime, &mut pending);
        assert!(matches!(
            outcome,
            super::PaneLaunchOutcome::Agent { operation, result: Ok(admission) }
                if operation == blocked && admission.terminal.fences(&launched)
        ));
        assert!(ui.active_pane_launch.is_none());
        assert!(!pending.contains_key(&blocked));
        assert!(pending.contains_key(&queued));

        super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
        assert_eq!(
            entered.recv_timeout(std::time::Duration::from_secs(10)),
            Ok("launch")
        );
        assert!(ui.pane_launches.is_empty());
        release.send(()).unwrap();
        let outcome = drain_next_completion(&mut ui, &mut runtime, &mut pending);
        assert!(matches!(
            outcome,
            super::PaneLaunchOutcome::Agent { operation, .. } if operation == queued
        ));
        assert!(pending.is_empty());
        // Two requests, two completions, and the stream port was never asked to
        // launch anything.
        assert_eq!(stream.lock().unwrap().launches, 0);
        assert!(ui.pane_completions.try_recv().is_err());
    }

    /// One request the identity-recording launch client answered.
    #[derive(Debug, Clone)]
    struct RecordedLaunch {
        kind: &'static str,
        operation: OperationId,
        terminal: TerminalRef,
    }

    /// A launch client that records the operation each request carried and mints a
    /// terminal for exactly that operation. A completion applied to the wrong
    /// pending pane is therefore visible as a foreign terminal on that pane.
    struct IdentityRecordingLaunchPort(Arc<Mutex<Vec<RecordedLaunch>>>);

    impl IdentityRecordingLaunchPort {
        fn record(
            &self,
            kind: &'static str,
            operation: OperationId,
            workspace: WorkspaceId,
            session: Option<SessionId>,
        ) -> TerminalRef {
            let terminal = scoped_terminal_ref(workspace, session);
            self.0.lock().unwrap().push(RecordedLaunch {
                kind,
                operation,
                terminal: terminal.clone(),
            });
            terminal
        }
    }

    impl PaneLaunchCommandPort for IdentityRecordingLaunchPort {
        fn launch(
            &self,
            operation: OperationId,
            workspace: WorkspaceId,
            session: Option<SessionId>,
            _profile: Option<AgentProfileId>,
        ) -> Result<AgentPaneAdmission, String> {
            Ok(AgentPaneAdmission {
                terminal: self.record("agent", operation, workspace, session),
                continuation: None,
            })
        }

        fn resume(
            &self,
            _workspace: WorkspaceId,
            _session: SessionId,
            _operation: OperationId,
        ) -> Result<AgentPaneAdmission, String> {
            Err("resume is not part of this fixture".to_owned())
        }

        fn resume_exact(
            &self,
            _target: AgentResumeTarget,
            _operation: OperationId,
        ) -> Result<ExactAgentResume, String> {
            Err("resume is not part of this fixture".to_owned())
        }

        fn launch_terminal(
            &self,
            workspace: WorkspaceId,
            session: Option<SessionId>,
            _geometry: Geometry,
            _arguments: &str,
            operation: OperationId,
        ) -> Result<TerminalRef, String> {
            Ok(self.record("terminal", operation, workspace, session))
        }
    }

    /// The live terminals `target`'s pane currently shows.
    fn live_tab_terminals(runtime: &WorkspaceRuntime, target: Target) -> Vec<TerminalRef> {
        runtime
            .panes()
            .pane(target)
            .map(|pane| {
                pane.tabs()
                    .iter()
                    .filter_map(|tab| match tab {
                        PaneTab::Live(live) => Some(live.terminal.clone()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// #522: the operation the controller issued for a pending pane is the one the
    /// launch client is asked with — Agent and generic terminal, workspace root and
    /// session alike. No adapter mints a second identity whose side effect could be
    /// promoted into this pane.
    #[test]
    fn every_pane_launch_request_carries_its_own_pending_operation() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut ui = ui_with_split_ports(
            workspace,
            session,
            Arc::new(Mutex::new(StreamCalls::default())),
            Box::new(IdentityRecordingLaunchPort(Arc::clone(&requests))),
        );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let mut pending = std::collections::HashMap::new();

        let root_agent = OperationId::new();
        let session_agent = OperationId::new();
        let session_terminal = OperationId::new();
        let planned = [
            (
                Target::Root(workspace),
                root_agent,
                "agent",
                super::PaneLaunch::Agent {
                    operation: root_agent,
                    workspace,
                    session: None,
                    profile: None,
                    resume: false,
                },
            ),
            (
                Target::Session(session),
                session_agent,
                "agent",
                agent_launch(workspace, session, session_agent),
            ),
            (
                Target::Session(session),
                session_terminal,
                "terminal",
                super::PaneLaunch::Terminal {
                    operation: session_terminal,
                    workspace,
                    session: Some(session),
                    arguments: "new".into(),
                },
            ),
        ];
        let expected = planned
            .iter()
            .map(|(target, operation, kind, _)| (*target, *operation, *kind))
            .collect::<Vec<_>>();
        for (target, operation, _, launch) in planned {
            runtime.request_pane(target, operation, PaneKind::Agent);
            pending.insert(operation, target);
            super::enqueue_pane_launch(&mut ui, launch);
        }

        // One worker at a time, each drained before the next is admitted.
        for (target, operation, kind) in expected {
            super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
            drain_next_completion(&mut ui, &mut runtime, &mut pending);
            let recorded = requests
                .lock()
                .unwrap()
                .last()
                .cloned()
                .expect("each admitted launch reaches the client once");
            assert_eq!(recorded.kind, kind);
            assert_eq!(
                recorded.operation, operation,
                "the pending pane's own operation is what the daemon is asked with"
            );
            assert!(
                live_tab_terminals(&runtime, target).contains(&recorded.terminal),
                "the pane promoted the terminal its own operation was answered with"
            );
        }
        assert_eq!(requests.lock().unwrap().len(), 3);
        assert!(pending.is_empty());
    }

    /// #522: while a pending operation lives in this process, its completion — even
    /// applied out of order — promotes only its own pane, and a completion that
    /// arrives after the pending tab is gone revives nothing.
    #[test]
    fn out_of_order_and_late_completions_never_cross_or_revive_a_pane() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let root = Target::Root(workspace);
        let scoped = Target::Session(session);
        let mut ui = ui_with_split_ports(
            workspace,
            session,
            Arc::new(Mutex::new(StreamCalls::default())),
            Box::new(IdentityRecordingLaunchPort(Arc::new(
                Mutex::new(Vec::new()),
            ))),
        );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let mut pending = std::collections::HashMap::new();

        let first = OperationId::new();
        let second = OperationId::new();
        let closed = OperationId::new();
        let first_terminal = scoped_terminal_ref(workspace, None);
        let second_terminal = scoped_terminal_ref(workspace, Some(session));
        let closed_terminal = scoped_terminal_ref(workspace, Some(session));
        for (target, operation) in [(root, first), (scoped, second), (scoped, closed)] {
            runtime.request_pane(target, operation, PaneKind::Agent);
            pending.insert(operation, target);
        }
        // The third pane is dropped before its daemon answer arrives.
        runtime.fail_pane(scoped, closed, "cancelled".to_owned());

        // The answers arrive in the reverse of the order they were requested.
        for (operation, terminal) in [
            (closed, closed_terminal.clone()),
            (second, second_terminal.clone()),
            (first, first_terminal.clone()),
        ] {
            ui.pane_completion_sender
                .send(super::PaneLaunchCompletion {
                    launch_id: super::PANE_LAUNCH_UNADMITTED,
                    outcome: super::PaneLaunchOutcome::Agent {
                        operation,
                        result: Ok(AgentPaneAdmission {
                            terminal,
                            continuation: None,
                        }),
                    },
                })
                .expect("the workspace still owns its completion receiver");
        }
        super::drain_pane_completions_into_runtime(
            &mut ui,
            &mut runtime,
            &mut pending,
            terminal_geometry(20, 80),
        );

        assert_eq!(live_tab_terminals(&runtime, root), vec![first_terminal]);
        assert_eq!(live_tab_terminals(&runtime, scoped), vec![second_terminal]);
        assert!(
            !live_tab_terminals(&runtime, scoped).contains(&closed_terminal),
            "a completion for a closed pending tab never revives it"
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn a_panicking_launch_worker_fails_only_its_pane() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let live = live_terminal_ref(workspace, session);
        let launched = scoped_terminal_ref(workspace, Some(session));
        let stream = Arc::new(Mutex::new(StreamCalls::default()));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut ui = ui_with_split_ports(
            workspace,
            session,
            Arc::clone(&stream),
            Box::new(PanickingLaunchPort {
                terminal: launched.clone(),
                calls: Arc::clone(&calls),
            }),
        );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        ui.start_terminal_session(live.clone(), terminal_geometry(20, 80));
        let dying = OperationId::new();
        let next = OperationId::new();
        let mut pending = std::collections::HashMap::new();
        for operation in [dying, next] {
            runtime.on_effect(&Effect::LaunchAgent {
                workspace,
                session: Some(session),
                operation_id: operation,
                profile: None,
            });
            pending.insert(operation, target);
            super::enqueue_pane_launch(&mut ui, agent_launch(workspace, session, operation));
        }

        // The worker unwinds inside the client. Its pane still gets exactly one
        // safe failure, and the shared client is not lost with the thread.
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
        let outcome = drain_next_completion(&mut ui, &mut runtime, &mut pending);
        std::panic::set_hook(hook);
        assert!(matches!(
            outcome,
            super::PaneLaunchOutcome::Agent { operation, result: Err(message) }
                if operation == dying && message == super::PANE_LAUNCH_WORKER_FAILED
        ));
        assert!(ui.active_pane_launch.is_none());
        assert!(!pending.contains_key(&dying));

        // The live pane never noticed, and the next launch reaches the daemon.
        assert!(ui.poll_all_terminals().is_empty());
        assert_eq!(ui.send_terminal_bytes(&live, b"c"), Ok(()));
        super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
        let outcome = drain_next_completion(&mut ui, &mut runtime, &mut pending);
        assert!(matches!(
            outcome,
            super::PaneLaunchOutcome::Agent { operation, result: Ok(admission) }
                if operation == next && admission.terminal.fences(&launched)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(pending.is_empty());
        assert_eq!(stream.lock().unwrap().launches, 0);
    }

    #[test]
    fn launch_admission_is_bounded_and_refuses_beyond_the_queue_with_one_busy_completion() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let launched = scoped_terminal_ref(workspace, Some(session));
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let (finished_tx, _finished) = std::sync::mpsc::channel();
        let stream = Arc::new(Mutex::new(StreamCalls::default()));
        let mut ui = ui_with_split_ports(
            workspace,
            session,
            Arc::clone(&stream),
            Box::new(GatedLaunchPort {
                terminal: launched,
                entered: Mutex::new(entered_tx),
                release: Mutex::new(release_rx),
                finished: Mutex::new(finished_tx),
            }),
        );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let mut pending = std::collections::HashMap::new();
        let mut admitted = Vec::new();
        for _ in 0..super::PANE_LAUNCH_QUEUE_LIMIT {
            let operation = OperationId::new();
            admitted.push(operation);
            super::enqueue_pane_launch(&mut ui, agent_launch(workspace, session, operation));
        }
        assert_eq!(ui.pane_launches.len(), super::PANE_LAUNCH_QUEUE_LIMIT);

        // The queue is full: the next requests never reach the daemon and each
        // completes exactly once as Busy — Agent, generic terminal, and explicit
        // tab resume alike.
        let history = interrupted_history(workspace, Some(session), true);
        let refused_kinds = [
            agent_launch(workspace, session, OperationId::new()),
            super::PaneLaunch::Terminal {
                operation: OperationId::new(),
                workspace,
                session: Some(session),
                arguments: "new".into(),
            },
            super::PaneLaunch::ResumeExact {
                operation: OperationId::new(),
                continuation: history.continuation,
                target: history.target.clone().unwrap(),
            },
        ];
        let refused = refused_kinds
            .iter()
            .map(|launch| launch.identity().operation())
            .collect::<Vec<_>>();
        for (launch, operation) in refused_kinds.into_iter().zip(refused.iter().copied()) {
            runtime.on_effect(&Effect::LaunchAgent {
                workspace,
                session: Some(session),
                operation_id: operation,
                profile: None,
            });
            pending.insert(operation, target);
            super::enqueue_pane_launch(&mut ui, launch);
        }
        assert_eq!(ui.pane_launches.len(), super::PANE_LAUNCH_QUEUE_LIMIT);

        // One worker is admitted first, so the Busy completions must not free it.
        super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
        assert_eq!(
            entered.recv_timeout(std::time::Duration::from_secs(10)),
            Ok("launch")
        );
        let active = ui.active_pane_launch;
        assert!(active.is_some());
        let outcomes = drain_completions(&mut ui, &mut runtime, &mut pending, refused.len());
        for (outcome, operation) in outcomes.iter().zip(refused.iter().copied()) {
            let (completed, message) = match outcome {
                super::PaneLaunchOutcome::Agent { operation, result } => {
                    (*operation, result.as_ref().err().cloned())
                }
                super::PaneLaunchOutcome::Terminal { operation, result } => {
                    (*operation, result.as_ref().err().cloned())
                }
                super::PaneLaunchOutcome::ResumeExact {
                    operation, result, ..
                } => (*operation, result.as_ref().err().cloned()),
            };
            assert_eq!(completed, operation);
            assert_eq!(message.as_deref(), Some(super::PANE_LAUNCH_BUSY));
        }
        // A Busy completion is unadmitted: it never frees the running worker.
        assert_eq!(ui.active_pane_launch, active);
        assert!(pending.is_empty());
        assert!(ui.pane_completions.try_recv().is_err());
        release.send(()).unwrap();
    }

    #[test]
    fn a_stale_completion_neither_frees_admission_nor_completes_another_pane() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let stale_terminal = scoped_terminal_ref(workspace, Some(session));
        let stream = Arc::new(Mutex::new(StreamCalls::default()));
        let mut ui = ui_with_split_ports(
            workspace,
            session,
            Arc::clone(&stream),
            Box::new(UnavailablePaneLaunchPort),
        );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let waiting = OperationId::new();
        runtime.on_effect(&Effect::LaunchAgent {
            workspace,
            session: Some(session),
            operation_id: waiting,
            profile: None,
        });
        let mut pending = std::collections::HashMap::from([(waiting, target)]);
        // A newer worker owns admission.
        ui.active_pane_launch = Some(7);

        // A completion from an older worker, for an operation nobody waits for.
        ui.pane_completion_sender
            .send(super::PaneLaunchCompletion {
                launch_id: 3,
                outcome: super::PaneLaunchOutcome::Agent {
                    operation: OperationId::new(),
                    result: Ok(AgentPaneAdmission {
                        terminal: stale_terminal,
                        continuation: None,
                    }),
                },
            })
            .unwrap();
        super::drain_pane_completions_into_runtime(
            &mut ui,
            &mut runtime,
            &mut pending,
            terminal_geometry(20, 80),
        );

        assert_eq!(ui.active_pane_launch, Some(7));
        assert_eq!(pending.get(&waiting), Some(&target));
        assert_eq!(runtime.focused_terminal(), None);
    }

    #[test]
    fn a_workspace_exit_never_strands_the_shared_launch_client() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let launched = scoped_terminal_ref(workspace, Some(session));
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished) = std::sync::mpsc::channel();
        let stream = Arc::new(Mutex::new(StreamCalls::default()));
        let mut ui = ui_with_split_ports(
            workspace,
            session,
            Arc::clone(&stream),
            Box::new(GatedLaunchPort {
                terminal: launched,
                entered: Mutex::new(entered_tx),
                release: Mutex::new(release_rx),
                finished: Mutex::new(finished_tx),
            }),
        );
        super::enqueue_pane_launch(
            &mut ui,
            agent_launch(workspace, session, OperationId::new()),
        );
        super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
        assert_eq!(
            entered.recv_timeout(std::time::Duration::from_secs(10)),
            Ok("launch")
        );

        // The workspace exits while the worker is still inside the client: its
        // completion receiver is gone, so the send is dropped harmlessly and the
        // borrowed client outlives the UI instead of being lost with it.
        drop(ui);
        release.send(()).unwrap();
        assert_eq!(
            finished.recv_timeout(std::time::Duration::from_secs(10)),
            Ok("launch")
        );
        assert_eq!(stream.lock().unwrap().launches, 0);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One sequence fixes both completion kinds and persisted selection.
    fn successful_pane_completions_persist_focus_and_select_agent_tabs() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let agent_terminal = scoped_terminal_ref(workspace, Some(session));
        let generic_terminal = scoped_terminal_ref(workspace, Some(session));
        let continuation = AgentContinuationRef::new();
        let durable = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
        let mutations = Arc::new(Mutex::new(Vec::new()));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(
                workspace,
                vec![session],
                Box::new(SuccessfulAgentPort(agent_terminal.clone())),
            )
            .with_agent_tab_intent(
                workspace,
                BTreeSet::from([session]),
                Box::new(MemoryIntentPort {
                    state: Arc::clone(&durable),
                    mutations: Arc::clone(&mutations),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let mut pending = std::collections::HashMap::new();
        let _ = runtime.apply_event(AppEvent::Key(AppKey::Enter));
        assert_eq!(runtime.panes().active(), Some(Target::Session(session)));

        let agent_operation = OperationId::new();
        runtime.on_effect(&Effect::LaunchAgent {
            workspace,
            session: Some(session),
            operation_id: agent_operation,
            profile: None,
        });
        pending.insert(agent_operation, target);
        ui.pane_completion_sender
            .send(super::PaneLaunchCompletion {
                launch_id: super::PANE_LAUNCH_UNADMITTED,
                outcome: super::PaneLaunchOutcome::Agent {
                    operation: agent_operation,
                    result: Ok(AgentPaneAdmission {
                        terminal: agent_terminal.clone(),
                        continuation: Some(continuation),
                    }),
                },
            })
            .unwrap();
        super::drain_pane_completions_into_runtime(
            &mut ui,
            &mut runtime,
            &mut pending,
            Geometry { cols: 20, rows: 5 },
        );
        assert_eq!(runtime.focused_terminal(), Some(agent_terminal.clone()));
        assert!(
            durable.lock().unwrap().targets[0].tabs[0]
                .terminal
                .fences(&agent_terminal)
        );

        let terminal_operation = OperationId::new();
        runtime.on_effect(&Effect::OpenTerminal {
            target,
            operation_id: terminal_operation,
            arguments: "new".into(),
        });
        pending.insert(terminal_operation, target);
        ui.pane_completion_sender
            .send(super::PaneLaunchCompletion {
                launch_id: super::PANE_LAUNCH_UNADMITTED,
                outcome: super::PaneLaunchOutcome::Terminal {
                    operation: terminal_operation,
                    result: Ok(generic_terminal.clone()),
                },
            })
            .unwrap();
        super::drain_pane_completions_into_runtime(
            &mut ui,
            &mut runtime,
            &mut pending,
            Geometry { cols: 20, rows: 5 },
        );
        assert!(pending.is_empty());
        assert_eq!(runtime.focused_terminal(), Some(generic_terminal));

        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(ControllerHostAction::SelectTab(TabDirection::Previous))
            .unwrap();
        drain_host_actions(&receiver, &mut ui, &mut runtime, &mut pending);
        assert_eq!(runtime.focused_terminal(), Some(agent_terminal));
        assert!(matches!(
            mutations.lock().unwrap().last(),
            Some(AgentTabIntentMutation::Select {
                session_id: Some(actual),
                continuation: Some(actual_continuation),
            }) if *actual == session && *actual_continuation == continuation
        ));
    }

    #[test]
    fn drawer_new_root_completion_commits_one_selected_exact_tab_across_reopen() {
        let workspace = WorkspaceId::new();
        let terminal = scoped_terminal_ref(workspace, None);
        let continuation = AgentContinuationRef::new();
        let durable = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
        let mutations = Arc::new(Mutex::new(Vec::new()));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(workspace, Vec::new(), Box::new(UnavailableAgentCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::new(),
                Box::new(MemoryIntentPort {
                    state: Arc::clone(&durable),
                    mutations: Arc::clone(&mutations),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
        runtime.set_agent_models(
            AvailableModels::new([DefaultModel::Claude]),
            DefaultModel::Claude,
        );
        let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
        assert!(
            runtime
                .handle_key(Key::Live(LiveTerminalAction::DirectorNew))
                .is_empty()
        );
        let effects = runtime.handle_key(Key::Enter);
        let [
            effect @ Effect::LaunchAgent {
                session: None,
                operation_id,
                profile: Some(profile),
                ..
            },
        ] = effects.as_slice()
        else {
            panic!("drawer confirmation must emit one explicit root launch: {effects:?}");
        };
        assert_eq!(profile.as_str(), "claude");
        runtime.on_effect(effect);
        let mut pending =
            std::collections::HashMap::from([(*operation_id, Target::Root(workspace))]);

        ui.pane_completion_sender
            .send(super::PaneLaunchCompletion {
                launch_id: super::PANE_LAUNCH_UNADMITTED,
                outcome: super::PaneLaunchOutcome::Agent {
                    operation: *operation_id,
                    result: Ok(AgentPaneAdmission {
                        terminal: terminal.clone(),
                        continuation: Some(continuation),
                    }),
                },
            })
            .unwrap();
        super::drain_pane_completions_into_runtime(
            &mut ui,
            &mut runtime,
            &mut pending,
            Geometry { cols: 20, rows: 5 },
        );

        assert!(pending.is_empty());
        assert_eq!(runtime.state().director_launching(), None);
        assert_eq!(runtime.focused_terminal(), Some(terminal.clone()));
        let intent = durable.lock().unwrap();
        assert_eq!(intent.targets.len(), 1);
        assert_eq!(intent.targets[0].session_id, None);
        assert_eq!(intent.targets[0].selected, Some(continuation));
        assert_eq!(intent.targets[0].tabs.len(), 1);
        assert!(intent.targets[0].tabs[0].terminal.fences(&terminal));
        drop(intent);

        let tabs = runtime.active_pane().tabs().to_vec();
        let _ = runtime.handle_key(Key::Escape);
        assert!(!runtime.state().director_drawer_open());
        let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
        assert_eq!(runtime.active_pane().tabs(), tabs.as_slice());
        assert_eq!(
            mutations
                .lock()
                .unwrap()
                .iter()
                .filter(|mutation| matches!(
                    mutation,
                    AgentTabIntentMutation::Upsert {
                        session_id: None,
                        continuation: actual,
                        select: true,
                        ..
                    } if *actual == continuation
                ))
                .count(),
            1
        );
    }

    #[test]
    fn drawer_root_final_without_conversation_identity_fails_closed() {
        let workspace = WorkspaceId::new();
        let durable = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
        let mutations = Arc::new(Mutex::new(Vec::new()));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::new(),
                Box::new(MemoryIntentPort {
                    state: Arc::clone(&durable),
                    mutations: Arc::clone(&mutations),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
        let operation = OperationId::new();
        let target = Target::Root(workspace);
        runtime.on_effect(&Effect::LaunchAgent {
            workspace,
            session: None,
            operation_id: operation,
            profile: Some(AgentProfileId::new("codex").unwrap()),
        });
        let mut pending = std::collections::HashMap::from([(operation, target)]);
        ui.pane_completion_sender
            .send(super::PaneLaunchCompletion {
                launch_id: super::PANE_LAUNCH_UNADMITTED,
                outcome: super::PaneLaunchOutcome::Agent {
                    operation,
                    result: Ok(AgentPaneAdmission {
                        terminal: scoped_terminal_ref(workspace, None),
                        continuation: None,
                    }),
                },
            })
            .unwrap();
        super::drain_pane_completions_into_runtime(
            &mut ui,
            &mut runtime,
            &mut pending,
            Geometry { cols: 20, rows: 5 },
        );

        assert!(pending.is_empty());
        assert!(live_tab_terminals(&runtime, target).is_empty());
        assert!(durable.lock().unwrap().targets.is_empty());
        assert!(mutations.lock().unwrap().is_empty());
    }

    #[test]
    fn select_tab_host_action_is_inert_without_an_active_target_or_tabs() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut pending = std::collections::HashMap::new();

        for mut runtime in [
            WorkspaceRuntime::new(workspace, Vec::new()),
            WorkspaceRuntime::new(workspace, vec![session]),
        ] {
            let view = WorkspaceView::with_runtime_ids(
                ws("demo"),
                state("demo"),
                runtime.state().sessions().to_vec(),
            );
            let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
            let (sender, receiver) = std::sync::mpsc::channel();
            sender
                .send(ControllerHostAction::SelectTab(TabDirection::Next))
                .unwrap();
            drain_host_actions(&receiver, &mut ui, &mut runtime, &mut pending);
            assert!(runtime.focused_terminal().is_none());
        }
    }

    #[test]
    fn compatibility_ports_fail_explicitly_and_never_silently_succeed() {
        struct DefaultSessionPort;
        impl SessionCommandPort for DefaultSessionPort {}

        let workspace_id = WorkspaceId::new();
        let session_id = SessionId::new();
        let workspace = ws("fallback");
        assert!(
            DefaultSessionPort
                .execute(&workspace, None, SessionCommand::List)
                .is_err()
        );
        assert!(
            UnavailableSessionCommandPort
                .execute(&workspace, None, SessionCommand::List)
                .is_err()
        );
        assert!(
            UnavailableAgentCommandPort
                .launch(OperationId::new(), workspace_id, None, None)
                .is_err()
        );
        // An embedder without a launch client refuses every pane launch inline
        // instead of leaving a pending tab forever.
        let history = interrupted_history(workspace_id, Some(session_id), true);
        assert!(
            UnavailablePaneLaunchPort
                .launch(OperationId::new(), workspace_id, None, None)
                .is_err()
        );
        assert!(
            UnavailablePaneLaunchPort
                .resume(workspace_id, session_id, OperationId::new())
                .is_err()
        );
        assert!(
            UnavailablePaneLaunchPort
                .resume_exact(history.target.unwrap(), OperationId::new())
                .is_err()
        );
        assert!(
            UnavailablePaneLaunchPort
                .launch_terminal(
                    workspace_id,
                    None,
                    terminal_geometry(20, 80),
                    "new",
                    OperationId::new(),
                )
                .is_err()
        );

        let decision_id = UserDecisionId::new();
        assert!(matches!(
            UnavailableDecisionCommandPort.refresh(workspace_id),
            BackendEvent::Notice(_)
        ));
        assert!(matches!(
            UnavailableDecisionCommandPort.resolve(
                workspace_id,
                decision_id,
                UserDecisionAnswer::Option {
                    option_id: "safe".to_owned(),
                },
            ),
            BackendEvent::DecisionError { .. }
        ));
        assert!(matches!(
            UnavailableEnvironmentStore.load(EnvScope::Workspace),
            BackendEvent::EnvironmentError { .. }
        ));
        assert!(matches!(
            UnavailableEnvironmentStore.save(EnvScope::Global, Vec::new()),
            BackendEvent::EnvironmentError { .. }
        ));
        assert!(UnavailablePrSnapshotPort.snapshot(session_id).is_err());
        assert!(
            UnavailableBrowserOpener
                .open("https://example.com")
                .is_err()
        );
        NoDesktopNotifications.notify("title", "body");

        let mut settings = DefaultSettingsPort;
        settings
            .save(SettingsScope::Global, &Settings::default())
            .unwrap();
    }

    type SessionCommandCall = (String, Option<String>, SessionCommand);

    struct RecordingExternalTerminalPort(Arc<Mutex<Vec<PathBuf>>>);

    impl ExternalTerminalPort for RecordingExternalTerminalPort {
        fn open(&mut self, directory: &Path) -> Result<(), String> {
            self.0.lock().unwrap().push(directory.to_path_buf());
            Ok(())
        }
    }

    #[test]
    fn unavailable_external_terminal_port_returns_a_safe_error() {
        assert_eq!(
            UnavailableExternalTerminalPort.open(Path::new("/tmp/worktree")),
            Err("external terminal launch is unavailable".to_owned())
        );
    }

    #[test]
    fn external_terminal_launch_does_not_require_agent_port() {
        let workspace = WorkspaceId::new();
        let view =
            WorkspaceView::with_runtime_ids(ws("demo"), WorkspaceState::default(), Vec::new());
        let opened = Arc::new(Mutex::new(Vec::new()));
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_external_terminal(Box::new(RecordingExternalTerminalPort(opened.clone())));
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(ControllerHostAction::OpenExternalTerminal(Target::Root(
                workspace,
            )))
            .unwrap();

        drain_host_actions(
            &receiver,
            &mut ui,
            &mut runtime,
            &mut std::collections::HashMap::new(),
        );

        assert_eq!(*opened.lock().unwrap(), vec![PathBuf::from("/tmp/demo")]);
    }

    /// Bind a fake as the dedicated pane-launch client, the way the composition
    /// root binds a second daemon client for launches. It is deliberately a
    /// different instance from the resident stream port.
    fn launch_port(port: Box<dyn AgentCommandPort>) -> Box<dyn PaneLaunchCommandPort> {
        Box::new(SerializedPaneLaunchPort::new(port))
    }

    struct SuccessfulAgentPort(TerminalRef);

    #[coverage(off)] // coverage: reason=generic_monomorphization owner=tui expires=2027-01-31 tests=production_screen_graph_fake_port_contract
    impl AgentCommandPort for SuccessfulAgentPort {
        fn launch(
            &mut self,
            _operation: OperationId,
            _workspace: WorkspaceId,
            _session: Option<SessionId>,
            _profile: Option<AgentProfileId>,
        ) -> Result<AgentPaneAdmission, String> {
            Ok(AgentPaneAdmission {
                terminal: self.0.clone(),
                continuation: None,
            })
        }
    }

    /// screen graph の workspace 遷移が実 port を通すことを検証する fake port。
    /// `session create <name>` に対しては、daemon lifecycle snapshot を模して
    /// `name` の session row を返し、sidebar への反映まで観測できるようにする。
    #[derive(Clone)]
    struct SnapshotSessionPort(Arc<Mutex<Vec<SessionCommandCall>>>);

    #[coverage(off)] // coverage: reason=generic_monomorphization owner=tui expires=2027-01-31 tests=production_screen_graph_fake_port_contract
    impl SessionCommandPort for SnapshotSessionPort {
        fn execute(
            &self,
            workspace: &Workspace,
            selected: Option<&SessionRecord>,
            command: SessionCommand,
        ) -> Result<SessionCommandResult, String> {
            let sessions = match &command {
                SessionCommand::Create { name, .. } => Some(vec![SessionRecord {
                    name: name.clone(),
                    display_name: None,
                    origin: SessionOrigin::Human,
                    started_from: None,
                    root: workspace.path.join(".usagi/sessions").join(name),
                    created_at: now(),
                    last_active: None,
                    notes: Scratchpad::default(),
                    prs: Vec::new(),
                }]),
                SessionCommand::Remove { .. } => Some(Vec::new()),
                _ => None,
            };
            self.0.lock().unwrap().push((
                workspace.name.clone(),
                selected.map(|session| session.name.clone()),
                command,
            ));
            Ok(SessionCommandResult {
                message: "daemon accepted".to_owned(),
                sessions,
                session_ids: None,
                agent_resumes: None,
                session_lifecycles: None,
                session_roles: None,
                revision: None,
            })
        }
    }

    /// workspace 起動ごとに [`SnapshotSessionPort`] を新しく作る fake factory。
    /// 記録した command 列と生成回数を共有し、全起動経路が実 port を fresh に
    /// 通していることを固定する。
    struct SnapshotSessionPortFactory {
        calls: Arc<Mutex<Vec<SessionCommandCall>>>,
        created: Arc<Mutex<usize>>,
    }

    impl SessionCommandPortFactory for SnapshotSessionPortFactory {
        fn create(&mut self) -> Box<dyn SessionCommandPort> {
            *self.created.lock().unwrap() += 1;
            Box::new(SnapshotSessionPort(self.calls.clone()))
        }
    }

    fn recent(name: &str) -> Recent {
        Recent::Workspace(WorkspaceOverview::new(ws(name), 1, 0, 0))
    }

    fn run(
        term: &mut dyn Terminal,
        workspaces: Vec<Workspace>,
        recent: Vec<Recent>,
        now: DateTime<Utc>,
        loader: &mut dyn WorkspaceLoader,
    ) -> io::Result<Exit> {
        run_from_start(term, workspaces, recent, now, Start::Welcome, loader)
    }

    #[test]
    fn render_controller_frame_composites_the_home_and_overlays() {
        use crate::presentation::views::workspace::ProjectedSession;
        use crate::presentation::workspace_runtime::WorkspaceRuntime;
        use crate::usecase::application::controller::{
            AppEvent, AppKey, Effect, Notice, OperationResult,
        };

        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let projected = ProjectedSession {
            id: session,
            label: "alpha".into(),
            detail: "fixture".into(),
            cwd: "/work/alpha".into(),
            last_modified: now(),
            has_notes: false,
            pr_summary: None,
            removing: false,
            agent_resume: None,
            lifecycle: usagi_core::domain::session_lifecycle::SessionLifecycle::Available,
            failure_stage: None,
            failure_summary: None,
            role_id: None,
            parent_session_id: None,
            organization_depth: 0,
        };
        let sessions = std::slice::from_ref(&projected);
        let git = std::collections::BTreeMap::new();
        let root = std::path::Path::new("/work");
        // Every case here composites the same Home geometry; only the runtime
        // and its session rows vary. Diagnostic health uses its unobserved
        // default so these assertions stay about the overlays.
        let frame = |runtime: &WorkspaceRuntime, sessions: &[ProjectedSession]| {
            render_controller_frame(
                20,
                80,
                runtime,
                "atlas",
                root,
                sessions,
                None,
                health(),
                &git,
                None,
                None,
            )
        };

        // Base Home frame: workspace name and session row render.
        let runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let base = frame(&runtime, sessions);
        assert!(base.join("\n").contains("atlas"));
        assert!(base.join("\n").contains("alpha"));

        // Create form: with no sessions a single Down reaches + new session. It
        // renders inline in the sidebar row (the typed name), not as a centered
        // "New session" modal.
        let mut creating = WorkspaceRuntime::new(workspace, Vec::new());
        let _ = creating.handle_key(Key::Down);
        let _ = creating.handle_key(Key::Enter);
        for character in ['b', 'e', 't', 'a'] {
            let _ = creating.handle_key(Key::Char(character));
        }
        let create = frame(&creating, &[]);
        assert!(create.join("\n").contains("beta"));
        assert!(!create.join("\n").contains("New session"));

        // Exit prompt overlay: the shared choice buttons and shortcut lines
        // render, defaulting to `quit` focused.
        let mut quitting = WorkspaceRuntime::new(workspace, vec![session]);
        let _ = quitting.apply_event(AppEvent::Key(AppKey::CtrlQ));
        let quit = frame(&quitting, sessions);
        let quit_text = quit.join("\n");
        assert!(quit_text.contains("Leave this workspace?"));
        assert!(quit_text.contains("[ welcome ]"));
        assert!(quit_text.contains("[ quit    ]"));
        assert!(quit_text.contains("[ stay    ]"));
        assert!(quit_text.contains("←→/Tab: move"));

        // The runtime's persisted Overview palette renders through this path.
        let mut palette = WorkspaceRuntime::new(workspace, vec![session]);
        let _ = palette.handle_key(Key::Char(':'));
        let overview = frame(&palette, sessions);
        assert!(overview.join("\n").contains("Overview"));

        let _ = palette.apply_event(AppEvent::Key(AppKey::SubmitOverview(
            "roles workspace".to_owned(),
        )));
        let _ = palette.apply_event(AppEvent::Backend(BackendEvent::RolesLoaded {
            scope: RoleEditorScope::Workspace,
            source: "version = 1\n".to_owned(),
        }));
        let roles = frame(&palette, sessions);
        assert!(roles.join("\n").contains("workspace roles.toml"));

        // Create-failure dialog: a failed create OperationResult opens it, and
        // this path composites the safe message over Home.
        let mut failing = WorkspaceRuntime::new(workspace, Vec::new());
        let _ = failing.handle_key(Key::Down);
        let _ = failing.handle_key(Key::Enter);
        for character in ['a', 'p', 'i'] {
            let _ = failing.handle_key(Key::Char(character));
        }
        let token = match &failing.handle_key(Key::Enter)[..] {
            [Effect::CreateSession { token, .. }] => *token,
            other => panic!("expected a create effect, got {other:?}"),
        };
        let _ = failing.apply_event(AppEvent::OperationResult(OperationResult {
            token,
            succeeded: false,
            created: None,
            notice: Some(Notice::new("worktree path already exists")),
        }));
        let failure = frame(&failing, &[]);
        assert!(failure.join("\n").contains("Session create failed"));
        assert!(failure.join("\n").contains("worktree path already exists"));
    }

    #[test]
    fn closeup_environment_editor_is_composited_over_home() {
        use crate::presentation::views::workspace::ProjectedSession;
        use crate::presentation::workspace_runtime::WorkspaceRuntime;
        use crate::usecase::application::controller::{AppEvent, Effect};

        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let _ = runtime.handle_key(Key::Enter);
        for _ in 0..3 {
            let _ = runtime.handle_key(Key::Down);
        }
        assert!(matches!(
            runtime.handle_key(Key::Enter).as_slice(),
            [Effect::LoadEnvironment {
                scope: EnvScope::Workspace
            }]
        ));
        let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::EnvironmentLoaded {
            scope: EnvScope::Workspace,
            entries: Vec::new(),
            inherited: Vec::new(),
        }));
        let sessions = [ProjectedSession {
            id: session,
            label: "alpha".into(),
            detail: "fixture".into(),
            cwd: "/work/alpha".into(),
            last_modified: now(),
            has_notes: false,
            pr_summary: None,
            removing: false,
            agent_resume: None,
            lifecycle: usagi_core::domain::session_lifecycle::SessionLifecycle::Available,
            failure_stage: None,
            failure_summary: None,
            role_id: None,
            parent_session_id: None,
            organization_depth: 0,
        }];
        let frame = render_controller_frame(
            20,
            80,
            &runtime,
            "atlas",
            std::path::Path::new("/work"),
            &sessions,
            None,
            health(),
            &BTreeMap::new(),
            None,
            None,
        )
        .join("\n");
        assert!(frame.contains("Environment"));
        assert!(frame.contains("workspace env only (global values stay unchanged)"));
        assert!(frame.contains("one NAME=value binding per line"));
    }

    #[test]
    fn unavailable_backend_reports_role_editor_errors_for_load_and_save() {
        use crate::usecase::application::daemon_backend::TargetStorePort as _;

        let mut port = UnavailableBackendPort;
        let (load, load_events) = Completions::channel();
        port.load_roles(RoleEditorScope::Workspace, load);
        assert!(matches!(
            load_events.recv().unwrap(),
            AppEvent::Backend(BackendEvent::RolesError {
                scope: RoleEditorScope::Workspace,
                ..
            })
        ));

        let (save, save_events) = Completions::channel();
        port.save_roles(RoleEditorScope::Global, "version = 1\n".to_owned(), save);
        assert!(matches!(
            save_events.recv().unwrap(),
            AppEvent::Backend(BackendEvent::RolesError {
                scope: RoleEditorScope::Global,
                ..
            })
        ));
    }

    #[test]
    fn unavailable_backend_reports_pr_copy_and_dismiss_errors() {
        use crate::usecase::application::daemon_backend::OverlayPort as _;

        let mut port = UnavailableBackendPort;
        let (copy, copy_events) = Completions::channel();
        port.copy_pull_request("https://github.com/o/r/pull/7".to_owned(), copy);
        assert!(matches!(
            copy_events.recv().unwrap(),
            AppEvent::Backend(BackendEvent::Notice(_))
        ));

        let (dismiss, dismiss_events) = Completions::channel();
        port.dismiss_pull_request(
            SessionId::new(),
            "https://github.com/o/r/pull/7".to_owned(),
            dismiss,
        );
        assert!(matches!(
            dismiss_events.recv().unwrap(),
            AppEvent::Backend(BackendEvent::Notice(_))
        ));
    }

    #[test]
    fn session_role_catalog_filters_scope_and_falls_back_on_invalid_source() {
        let root = tempdir().unwrap();
        let data_home = root.path().join("home");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&data_home).unwrap();
        std::fs::write(
            data_home.join("roles.toml"),
            "version = 1\n[defaults]\nsession = \"coder\"\n[roles.coder]\nsummary = \"Code\"\nscopes = [\"session\"]\ninstructions = \"code\"\n[roles.director]\nsummary = \"Direct\"\nscopes = [\"root\"]\ninstructions = \"direct\"\n",
        )
        .unwrap();

        let catalog = super::session_role_catalog(Some(&data_home), &workspace);
        assert_eq!(catalog.default.unwrap().as_str(), "coder");
        assert_eq!(catalog.roles.len(), 1);
        assert_eq!(catalog.roles[0].id.as_str(), "coder");

        std::fs::write(data_home.join("roles.toml"), "version = 99\n").unwrap();
        assert_eq!(
            super::session_role_catalog(Some(&data_home), &workspace),
            SessionRoleCatalog::default()
        );
        assert_eq!(
            super::session_role_catalog(None, &workspace),
            SessionRoleCatalog::default()
        );
    }

    #[test]
    fn render_controller_frame_draws_a_waving_pending_create_skeleton() {
        // Once a create request is in flight, the shell threads its name here and
        // the sidebar draws a two-line loading skeleton just above `+ new
        // session` (document/03-tui.md). The sweep paints each cell with its own
        // SGR run, so compare on ANSI-stripped text.
        let strip = |frame: &[String]| {
            frame
                .iter()
                .map(|line| {
                    let mut out = String::new();
                    let mut chars = line.chars();
                    while let Some(ch) = chars.next() {
                        if ch == '\u{1b}' {
                            for c in chars.by_ref() {
                                if ('\u{40}'..='\u{7e}').contains(&c) && c != '[' {
                                    break;
                                }
                            }
                        } else {
                            out.push(ch);
                        }
                    }
                    out
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let workspace = WorkspaceId::new();
        let git = std::collections::BTreeMap::new();
        let root = std::path::Path::new("/work");

        let idle = WorkspaceRuntime::new(workspace, Vec::new());
        let pending = render_controller_frame(
            20,
            80,
            &idle,
            "atlas",
            root,
            &[],
            None,
            health(),
            &git,
            None,
            Some("beta"),
        );
        let pending_text = strip(&pending);
        assert!(pending_text.contains("+ beta"));
        assert!(pending_text.contains("creating"));

        // No pending create means no skeleton or loading caption.
        let quiet = render_controller_frame(
            20,
            80,
            &idle,
            "atlas",
            root,
            &[],
            None,
            health(),
            &git,
            None,
            None,
        );
        let quiet_text = strip(&quiet);
        assert!(!quiet_text.contains("beta"));
        assert!(!quiet_text.contains("creating"));

        // The wave advances with the mascot tick rather than blinking statically.
        let mut ticked = WorkspaceRuntime::new(workspace, Vec::new());
        for _ in 0..12 {
            let _ = ticked.apply_event(AppEvent::Tick);
        }
        let pending_ticked = render_controller_frame(
            20,
            80,
            &ticked,
            "atlas",
            root,
            &[],
            None,
            health(),
            &git,
            None,
            Some("beta"),
        );
        assert_ne!(pending, pending_ticked);
    }

    #[test]
    fn controller_loop_renders_home_and_detaches_on_quit_confirmation() {
        let snapshot = snapshot("demo");
        let terminal = TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: snapshot.workspace_id,
            session_id: snapshot.session_ids.first().copied(),
            worktree_id: WorktreeId::new(),
        };
        // Ctrl-Q opens the quit confirmation; `y` detaches and ends the loop.
        let mut term = FakeTerminal::with_keys(&[Key::CtrlQ, Key::Char('y')]);
        let result = run_workspace_controller(
            &mut term,
            snapshot,
            Box::new(UnavailableSessionCommandPort),
            Box::new(SuccessfulAgentPort(terminal.clone())),
            launch_port(Box::new(SuccessfulAgentPort(terminal))),
            Box::new(UnavailableDecisionCommandPort),
            Box::new(UnavailableEnvironmentStore),
            Box::new(NoDesktopNotifications),
            Box::new(NoMetrics),
            Box::new(UnavailablePrSnapshotPort),
            Box::new(UnavailableBrowserOpener),
        );

        assert!(matches!(result, Ok(Exit::Quit)));
        // The controller Home frame renders through render_home, and the quit
        // confirmation is composited before the loop detaches.
        assert!(
            term.frames
                .iter()
                .any(|frame| frame.join("\n").contains("demo"))
        );
        assert!(
            term.frames
                .iter()
                .any(|frame| frame.join("\n").contains("Leave this workspace?"))
        );
        // Regression: the real Ctrl-Q frame carries the shared choice buttons and
        // the ←→/Tab shortcut, not the old free-text y/n prompt. Leaving and
        // quitting are separate buttons (#556).
        assert!(
            term.frames
                .iter()
                .any(|frame| frame.join("\n").contains("[ quit    ]")),
            "exit prompt frame is missing the [ quit ] button"
        );
        assert!(
            term.frames
                .iter()
                .any(|frame| frame.join("\n").contains("[ welcome ]")),
            "exit prompt frame is missing the [ welcome ] button"
        );
        assert!(
            term.frames
                .iter()
                .any(|frame| frame.join("\n").contains("[ stay    ]")),
            "exit prompt frame is missing the [ stay ] button"
        );
        assert!(
            term.frames
                .iter()
                .any(|frame| frame.join("\n").contains("←→/Tab: move")),
            "exit prompt frame is missing the move shortcut"
        );
    }

    struct BlockingRestorePort {
        entered: Sender<()>,
        release: Receiver<()>,
    }

    impl AgentCommandPort for BlockingRestorePort {
        fn launch(
            &mut self,
            _operation: OperationId,
            _workspace: WorkspaceId,
            _session: Option<SessionId>,
            _profile: Option<AgentProfileId>,
        ) -> Result<AgentPaneAdmission, String> {
            Err("launch is unavailable".to_owned())
        }

        fn list_terminals(&mut self) -> Result<Vec<TerminalInventoryEntry>, TerminalError> {
            let _ = self.entered.send(());
            self.release
                .recv()
                .map_err(|_| TerminalError::Unavailable)?;
            Err(TerminalError::Unavailable)
        }
    }

    struct QuitWhileRestoreBlockedTerminal {
        entered: Option<Receiver<()>>,
        keys: VecDeque<Key>,
        frames: Vec<Vec<String>>,
    }

    impl Terminal for QuitWhileRestoreBlockedTerminal {
        fn size(&mut self) -> io::Result<(usize, usize)> {
            Ok((20, 80))
        }

        fn draw(&mut self, frame: &[String]) -> io::Result<()> {
            self.frames.push(frame.to_vec());
            Ok(())
        }

        fn wait(&mut self, _duration: std::time::Duration) -> io::Result<()> {
            Ok(())
        }

        fn read_key(&mut self) -> io::Result<Key> {
            if let Some(entered) = self.entered.take() {
                entered
                    .recv_timeout(std::time::Duration::from_secs(1))
                    .map_err(|error| io::Error::other(error.to_string()))?;
            }
            self.keys
                .pop_front()
                .ok_or_else(|| io::Error::other("no more keys"))
        }
    }

    #[test]
    fn blocked_restore_inventory_never_blocks_render_or_quit() {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let mut term = QuitWhileRestoreBlockedTerminal {
            entered: Some(entered_rx),
            keys: VecDeque::from([Key::CtrlQ, Key::Char('y')]),
            frames: Vec::new(),
        };
        let mut factory = FixedBackendFactory {
            sessions: Some(Box::new(UnavailableSessionCommandPort)),
            agent: Some(Box::new(UnavailableAgentCommandPort)),
            launch: None,
            restore: Some(Box::new(BlockingRestorePort {
                entered: entered_tx,
                release: release_rx,
            })),
            metrics: Some(Box::new(NoMetrics)),
            browser: Some(Box::new(UnavailableBrowserOpener)),
            session_refresh: None,
            decisions: None,
            session_worktrees: None,
        };

        let started = std::time::Instant::now();
        let result = run_workspace_controller_with_backend(
            &mut term,
            snapshot("blocked-restore"),
            &mut factory,
        );
        let elapsed = started.elapsed();
        let _ = release_tx.send(());

        assert_eq!(result.unwrap(), Exit::Quit);
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "quit waited for a blocked restore worker: {elapsed:?}"
        );
        assert!(
            term.frames
                .iter()
                .any(|frame| frame.join("\n").contains("blocked-restore"))
        );
        assert!(
            term.frames
                .iter()
                .any(|frame| { frame.join("\n").contains("Leave this workspace?") })
        );
    }

    #[test]
    fn direct_controller_entry_uses_the_resolved_workspace_settings() {
        let mut term = FakeTerminal::with_keys(&[
            Key::Char(':'),
            Key::Char('i'),
            Key::Escape,
            Key::CtrlQ,
            Key::Char('y'),
        ]);
        let mut factory = FixedBackendFactory {
            sessions: Some(Box::new(UnavailableSessionCommandPort)),
            agent: Some(Box::new(UnavailableAgentCommandPort)),
            launch: None,
            restore: None,
            metrics: Some(Box::new(NoMetrics)),
            browser: Some(Box::new(UnavailableBrowserOpener)),
            session_refresh: None,
            decisions: None,
            session_worktrees: None,
        };
        let settings = usagi_core::domain::settings::Settings {
            modal_selection_mode: usagi_core::domain::settings::ModalSelectionMode::Prompt,
            ..usagi_core::domain::settings::Settings::default()
        };

        assert_eq!(
            run_workspace_controller_with_backend_and_settings(
                &mut term,
                snapshot("direct"),
                &mut factory,
                &settings,
            )
            .unwrap(),
            Exit::Quit
        );
        assert!(term.frames.iter().any(|frame| {
            let frame = frame.join("\n");
            frame.contains("Overview") && frame.contains("Enter: run   Esc: close")
        }));
    }

    /// #551 acceptance. The frame loop must be "non-blocking drain → projection
    /// → draw → input" and nothing else: neither a wake-up tick nor a resize may
    /// reach a daemon lane, and no frame may spawn a session worker. Both used
    /// to happen on every `Key::Other`, at 62.5Hz.
    #[test]
    fn ticks_and_resizes_never_reach_a_daemon_lane_or_spawn_a_session_worker() {
        let decision_wakes = Arc::new(AtomicUsize::new(0));
        let decision_polls = Arc::new(AtomicUsize::new(0));
        let lane_wakes = Arc::new(AtomicUsize::new(0));
        let lane_drains = Arc::new(AtomicUsize::new(0));
        let session_calls = Arc::new(Mutex::new(Vec::new()));

        // Forty wake-ups interleaved with forty resizes — the shape of dragging
        // a window edge while nothing else happens — then a modal open/close and
        // quit, all while both lanes stay silent.
        let mut keys = Vec::new();
        for _ in 0..40 {
            keys.push(Key::Other);
            keys.push(Key::Resize);
        }
        keys.extend([
            Key::Char(':'),
            Key::Char('i'),
            Key::Escape,
            Key::CtrlQ,
            Key::Char('y'),
        ]);
        let mut term = FakeTerminal::with_keys(&keys);
        let mut factory = FixedBackendFactory {
            sessions: Some(Box::new(SnapshotSessionPort(Arc::clone(&session_calls)))),
            agent: Some(Box::new(UnavailableAgentCommandPort)),
            launch: None,
            restore: None,
            metrics: Some(Box::new(NoMetrics)),
            browser: Some(Box::new(UnavailableBrowserOpener)),
            session_refresh: Some(Box::new(FakeSessionRefreshPort {
                wakes: Arc::clone(&lane_wakes),
                takes: Arc::clone(&lane_drains),
                queued: Arc::default(),
            })),
            decisions: Some(Box::new(CountingDecisionPort {
                wakes: Arc::clone(&decision_wakes),
                polls: Arc::clone(&decision_polls),
            })),
            session_worktrees: None,
        };

        assert_eq!(
            run_workspace_controller_with_backend(&mut term, snapshot("idle"), &mut factory)
                .unwrap(),
            Exit::Quit
        );

        // One seed wake per lane for the whole run — not one per frame.
        assert_eq!(decision_wakes.load(Ordering::SeqCst), 1);
        assert_eq!(lane_wakes.load(Ordering::SeqCst), 1);
        // The command port, and therefore `std::thread::spawn`, is untouched:
        // the tick no longer runs `SessionCommand::List`.
        assert!(
            session_calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
        // What the loop does do every frame is drain. Since #554 the redraw is
        // gated on the frame's material, so a tick that changes nothing draws
        // nothing — the per-iteration invariant lives in the drain counts, not
        // in the frame count.
        assert!(decision_polls.load(Ordering::SeqCst) >= 80);
        assert!(lane_drains.load(Ordering::SeqCst) >= 80);
        // Draw, modal, and quit all completed with both lanes never answering.
        assert!(
            term.frames
                .iter()
                .any(|frame| frame.join("\n").contains("Overview"))
        );
    }

    /// A worktree scan that counts how often the frame loop reaches the disk.
    struct CountingWorktreeScanPort {
        scans: Arc<AtomicUsize>,
        names: Vec<String>,
    }

    impl SessionWorktreeScanPort for CountingWorktreeScanPort {
        fn scan(&mut self, _workspace: &std::path::Path) -> Vec<String> {
            self.scans.fetch_add(1, Ordering::SeqCst);
            self.names.clone()
        }
    }

    fn counting_scan(scans: &Arc<AtomicUsize>) -> Box<dyn SessionWorktreeScanPort> {
        Box::new(CountingWorktreeScanPort {
            scans: Arc::clone(scans),
            names: vec!["stale-worktree".to_owned()],
        })
    }

    /// 16ms is the composition root's tick period, so this is "frame `tick`".
    fn at_tick(tick: u64) -> std::time::Duration {
        std::time::Duration::from_millis(16 * tick)
    }

    /// #554 acceptance. A closed create form has no reader for the hint, so the
    /// frame budget must not contain a `read_dir` at all — this used to be ~62
    /// directory scans per second plus one `stat` per entry, forever.
    #[test]
    fn a_closed_create_form_never_scans_the_sessions_directory() {
        let scans = Arc::new(AtomicUsize::new(0));
        let mut hint = SessionWorktreeHint::new(counting_scan(&scans));

        for tick in 0..600 {
            assert!(
                hint.names(false, std::path::Path::new("/tmp/demo"), at_tick(tick))
                    .is_empty(),
                "a closed form must contribute no hint"
            );
        }

        assert_eq!(scans.load(Ordering::SeqCst), 0);
    }

    /// #554 acceptance. An open form scans immediately — so the very first frame
    /// that shows the caret can already reject a known collision — and then no
    /// more than once per cadence period however long it stays open.
    #[test]
    fn an_open_create_form_scans_on_open_and_then_at_the_cadence_ceiling() {
        let scans = Arc::new(AtomicUsize::new(0));
        let mut hint = SessionWorktreeHint::new(counting_scan(&scans));
        let workspace = std::path::Path::new("/tmp/demo");

        // The frame that opens the form.
        assert_eq!(hint.names(true, workspace, at_tick(0)), ["stale-worktree"]);
        assert_eq!(scans.load(Ordering::SeqCst), 1);

        // Five seconds of 16ms ticks with the form left open.
        let ticks = 313;
        for tick in 1..ticks {
            assert_eq!(
                hint.names(true, workspace, at_tick(tick)),
                ["stale-worktree"]
            );
        }

        let elapsed = at_tick(ticks - 1);
        let ceiling =
            usize::try_from(1 + elapsed.as_millis() / SessionWorktreeHint::CADENCE.as_millis())
                .expect("the ceiling of a five second run fits a usize");
        let scanned = scans.load(Ordering::SeqCst);
        assert!(
            scanned <= ceiling,
            "{scanned} scans over {elapsed:?} exceeds the cadence ceiling of {ceiling}"
        );
        assert_eq!(scanned, 10);
    }

    /// Reopening the form is the moment the hint matters most, so it always
    /// rescans — even when the previous scan is still inside the cadence window.
    #[test]
    fn reopening_the_create_form_rescans_inside_the_cadence_window() {
        let scans = Arc::new(AtomicUsize::new(0));
        let mut hint = SessionWorktreeHint::new(counting_scan(&scans));
        let workspace = std::path::Path::new("/tmp/demo");

        assert_eq!(hint.names(true, workspace, at_tick(0)), ["stale-worktree"]);
        assert_eq!(hint.names(true, workspace, at_tick(1)), ["stale-worktree"]);
        assert_eq!(scans.load(Ordering::SeqCst), 1);

        assert!(hint.names(false, workspace, at_tick(2)).is_empty());
        assert_eq!(hint.names(true, workspace, at_tick(3)), ["stale-worktree"]);
        assert_eq!(scans.load(Ordering::SeqCst), 2);
    }

    /// #554 acceptance, through the real frame loop: an idle Home reaches
    /// neither the filesystem nor the renderer on a tick that changes nothing,
    /// while every drain still runs on exactly those ticks.
    #[test]
    fn idle_ticks_skip_the_worktree_scan_and_the_redraw_but_never_a_drain() {
        reset_projection_build_counts();
        let scans = Arc::new(AtomicUsize::new(0));
        let lane_drains = Arc::new(AtomicUsize::new(0));

        let ticks = 1_000;
        let mut keys = vec![Key::Other; ticks];
        keys.extend([Key::CtrlQ, Key::Char('y')]);
        let mut term = FakeTerminal::with_keys(&keys);
        let mut factory = FixedBackendFactory {
            sessions: Some(Box::new(UnavailableSessionCommandPort)),
            agent: Some(Box::new(UnavailableAgentCommandPort)),
            launch: None,
            restore: None,
            metrics: Some(Box::new(NoMetrics)),
            browser: Some(Box::new(UnavailableBrowserOpener)),
            session_refresh: Some(Box::new(FakeSessionRefreshPort {
                wakes: Arc::default(),
                takes: Arc::clone(&lane_drains),
                queued: Arc::default(),
            })),
            decisions: None,
            session_worktrees: Some(counting_scan(&scans)),
        };

        assert_eq!(
            run_workspace_controller_with_backend(&mut term, snapshot("idle"), &mut factory)
                .unwrap(),
            Exit::Quit
        );

        assert_eq!(
            scans.load(Ordering::SeqCst),
            0,
            "an idle frame reached the sessions directory"
        );
        let frames = term.frames.len();
        assert!(
            frames < ticks,
            "{frames} draws for {ticks} ticks: the redraw gate did nothing"
        );
        // The floor is the rabbit: it has three distinct appearances per six
        // ticks and #554 keeps that cadence, so roughly half the idle ticks are
        // genuinely material.
        assert!(
            frames <= ticks / 2 + 4,
            "{frames} draws for {ticks} ticks is above the animation floor"
        );
        // Every iteration still drained the resident lane, including the ones
        // that drew nothing.
        assert!(lane_drains.load(Ordering::SeqCst) >= ticks);
        let (session_builds, terminal_builds) = projection_build_counts();
        assert_eq!(
            session_builds, 1,
            "session rows/cwd were rebuilt on idle ticks"
        );
        assert_eq!(
            terminal_builds, 1,
            "terminal viewport/link projection was rebuilt on idle ticks"
        );
    }

    /// A terminal harness that records the projection generations visible at
    /// every actual draw. With `wait_for_builds`, it drives neutral ticks until
    /// the requested cache invalidation has happened, then quits. The condition
    /// is observable loop state rather than elapsed time; the finite ceiling is
    /// only a failure guard for a broken wiring under test.
    struct CacheInvalidationTerminal {
        keys: VecDeque<Key>,
        wait_for_builds: Option<(usize, usize)>,
        quit_started: bool,
        neutral_ticks: usize,
        frames: Vec<Vec<String>>,
        builds_at_draw: Vec<(usize, usize)>,
    }

    impl CacheInvalidationTerminal {
        fn scripted(keys: impl IntoIterator<Item = Key>) -> Self {
            Self {
                keys: keys.into_iter().collect(),
                wait_for_builds: None,
                quit_started: false,
                neutral_ticks: 0,
                frames: Vec::new(),
                builds_at_draw: Vec::new(),
            }
        }

        fn until_builds(keys: impl IntoIterator<Item = Key>, builds: (usize, usize)) -> Self {
            Self {
                wait_for_builds: Some(builds),
                ..Self::scripted(keys)
            }
        }
    }

    impl Terminal for CacheInvalidationTerminal {
        fn size(&mut self) -> io::Result<(usize, usize)> {
            Ok((20, 80))
        }

        fn draw(&mut self, frame: &[String]) -> io::Result<()> {
            self.frames.push(frame.to_vec());
            self.builds_at_draw.push(projection_build_counts());
            Ok(())
        }

        fn wait(&mut self, _duration: std::time::Duration) -> io::Result<()> {
            Ok(())
        }

        fn read_key(&mut self) -> io::Result<Key> {
            if let Some(key) = self.keys.pop_front() {
                return Ok(key);
            }
            let Some(expected) = self.wait_for_builds else {
                return Err(io::Error::other("no more cache-invalidation keys"));
            };
            let observed = projection_build_counts();
            if observed.0 >= expected.0 && observed.1 >= expected.1 {
                if self.quit_started {
                    return Ok(Key::Char('y'));
                }
                self.quit_started = true;
                return Ok(Key::Live(LiveTerminalAction::QuitConfirmation));
            }
            self.neutral_ticks += 1;
            if self.neutral_ticks >= 10_000 {
                return Err(io::Error::other(format!(
                    "cache invalidation was not observed: expected {expected:?}, got {observed:?}"
                )));
            }
            std::thread::yield_now();
            Ok(Key::Other)
        }

        fn copy_text(&mut self, _text: &str) -> Result<(), String> {
            Ok(())
        }
    }

    /// The session revision must cross both cache gates in the real composition
    /// loop: first rebuild the owned row/path projection, then rebuild and draw
    /// the frame that contains it.
    #[test]
    fn daemon_session_change_invalidates_the_joined_material_and_redraws() {
        reset_projection_build_counts();
        let snapshot = snapshot("session-cache");
        let original = snapshot.session_ids[0];
        let added = SessionId::new();
        let mut added_record = snapshot.state.sessions[0].clone();
        added_record.name = "cache-added".to_owned();
        added_record.root = PathBuf::from("/tmp/session-cache/cache-added");
        let update = SessionCommandResult {
            message: "daemon snapshot changed".to_owned(),
            sessions: Some(vec![snapshot.state.sessions[0].clone(), added_record]),
            session_ids: Some(vec![original, added]),
            agent_resumes: None,
            session_lifecycles: None,
            session_roles: None,
            revision: Some(1),
        };
        let mut term = CacheInvalidationTerminal::scripted([
            Key::Other,
            Key::Other,
            Key::Other,
            Key::CtrlQ,
            Key::Char('y'),
        ]);
        let mut factory = FixedBackendFactory {
            sessions: Some(Box::new(UnavailableSessionCommandPort)),
            agent: Some(Box::new(UnavailableAgentCommandPort)),
            launch: None,
            restore: None,
            metrics: Some(Box::new(NoMetrics)),
            browser: Some(Box::new(UnavailableBrowserOpener)),
            session_refresh: Some(Box::new(ScheduledSessionRefreshPort {
                publish_on_take: 3,
                takes: 0,
                update: Some(update),
            })),
            decisions: None,
            session_worktrees: None,
        };

        assert_eq!(
            run_workspace_controller_with_backend(&mut term, snapshot, &mut factory).unwrap(),
            Exit::Quit
        );

        let (session_builds, terminal_builds) = projection_build_counts();
        assert_eq!(session_builds, 2, "the changed session key did not rebuild");
        assert_eq!(terminal_builds, 1, "a session change rebuilt the terminal");
        assert!(
            term.builds_at_draw.contains(&(2, 1)),
            "the frame key did not redraw after the session material rebuild"
        );
        assert!(
            term.frames
                .iter()
                .any(|frame| frame.join("\n").contains("cache-added")),
            "the redrawn frame did not contain the changed session projection"
        );
    }

    struct ImmediateTerminalLaunchPort(TerminalRef);

    impl PaneLaunchCommandPort for ImmediateTerminalLaunchPort {
        fn launch(
            &self,
            _operation: OperationId,
            _workspace: WorkspaceId,
            _session: Option<SessionId>,
            _profile: Option<AgentProfileId>,
        ) -> Result<AgentPaneAdmission, String> {
            Err("agent launch is not scripted".to_owned())
        }

        fn resume(
            &self,
            _workspace: WorkspaceId,
            _session: SessionId,
            _operation: OperationId,
        ) -> Result<AgentPaneAdmission, String> {
            Err("agent resume is not scripted".to_owned())
        }

        fn resume_exact(
            &self,
            _target: usagi_core::domain::agent::AgentResumeTarget,
            _operation: OperationId,
        ) -> Result<super::ExactAgentResume, String> {
            Err("exact agent resume is not scripted".to_owned())
        }

        fn launch_terminal(
            &self,
            _workspace: WorkspaceId,
            _session: Option<SessionId>,
            _geometry: Geometry,
            _arguments: &str,
            _operation: OperationId,
        ) -> Result<TerminalRef, String> {
            Ok(self.0.clone())
        }
    }

    /// The resident stream starts with one checkpoint and publishes exactly one
    /// later output chunk. This changes the authoritative `TerminalSession` screen
    /// revision after the pane itself has already been projected once.
    struct ChangingTerminalPort {
        replay: Vec<u8>,
        empty_polls_before_update: usize,
        update: Option<Vec<u8>>,
    }

    impl AgentCommandPort for ChangingTerminalPort {
        fn launch(
            &mut self,
            _operation: OperationId,
            _workspace: WorkspaceId,
            _session: Option<SessionId>,
            _profile: Option<AgentProfileId>,
        ) -> Result<AgentPaneAdmission, String> {
            Err("agent launch is not scripted".to_owned())
        }

        fn attach_terminal(
            &mut self,
            _terminal: &TerminalRef,
            geometry: Geometry,
        ) -> Result<TerminalAttach, TerminalError> {
            Ok(TerminalAttach {
                subscription: TerminalSubscription { id: 1, epoch: 1 },
                revision: 1,
                output_offset: self.replay.len() as u64,
                next_input_seq: None,
                screen: attach_checkpoint(&self.replay, geometry),
                exited: false,
            })
        }

        fn poll_terminal(
            &mut self,
            _terminal: &TerminalRef,
            after_offset: u64,
        ) -> Result<Vec<TerminalChunk>, TerminalError> {
            if self.empty_polls_before_update > 0 {
                self.empty_polls_before_update -= 1;
                return Ok(Vec::new());
            }
            let Some(data) = self.update.take() else {
                return Ok(Vec::new());
            };
            Ok(vec![TerminalChunk {
                start_offset: after_offset,
                end_offset: after_offset + data.len() as u64,
                data,
            }])
        }
    }

    /// The actual controller path must propagate both a focused-pane change and
    /// a later terminal screen revision through `terminal_material_key` and the
    /// aggregate `FrameMaterialKey`. Each invalidation owes one viewport/link
    /// rebuild and one draw containing the new owned projection.
    #[test]
    fn terminal_output_change_invalidates_the_joined_material_and_redraws() {
        reset_projection_build_counts();
        let snapshot = snapshot("terminal-cache");
        let terminal = live_terminal_ref(snapshot.workspace_id, snapshot.session_ids[0]);
        let mut keys = vec![Key::Enter, Key::Live(LiveTerminalAction::OpenCloseupModal)];
        keys.extend("terminal open".chars().map(Key::Char));
        keys.push(Key::Enter);
        let mut term = CacheInvalidationTerminal::until_builds(keys, (1, 3));
        let mut factory = FixedBackendFactory {
            sessions: Some(Box::new(UnavailableSessionCommandPort)),
            agent: Some(Box::new(ChangingTerminalPort {
                replay: b"cache-before".to_vec(),
                empty_polls_before_update: 1,
                update: Some(b"\r\ncache-after".to_vec()),
            })),
            launch: Some(Box::new(ImmediateTerminalLaunchPort(terminal))),
            restore: None,
            metrics: Some(Box::new(NoMetrics)),
            browser: Some(Box::new(UnavailableBrowserOpener)),
            session_refresh: None,
            decisions: None,
            session_worktrees: None,
        };

        assert_eq!(
            run_workspace_controller_with_backend(&mut term, snapshot, &mut factory).unwrap(),
            Exit::Quit
        );

        let (session_builds, terminal_builds) = projection_build_counts();
        assert_eq!(session_builds, 1, "a terminal change rebuilt session rows");
        assert_eq!(
            terminal_builds, 3,
            "focused pane and screen revision did not each invalidate the terminal key"
        );
        for generation in [2, 3] {
            assert!(
                term.builds_at_draw.contains(&(1, generation)),
                "frame key did not redraw terminal generation {generation}"
            );
        }
        assert!(
            term.frames
                .iter()
                .any(|frame| frame.join("\n").contains("cache-after")),
            "the redraw did not contain output from the changed terminal screen"
        );
    }

    /// One admitted restore job and what the frame gate did on the tick that
    /// admitted it.
    #[derive(Debug)]
    struct RestoreAdmission {
        /// Each job runs on its own thread, so the thread id is what separates
        /// two admissions from the several inventory calls inside one of them.
        job: std::thread::ThreadId,
        /// Frames the terminal had drawn when this job started.
        drawn: usize,
        /// The admitting tick drew nothing: the frame count had not moved since
        /// that tick began.
        skipped: bool,
    }

    /// What the #554 skipped-tick acceptance observes. The frame gate runs on the
    /// loop thread and the admission is observed from the worker thread the job
    /// runs on, so both counts are shared: an admission is on a skipped tick
    /// when the frame count has not moved since that tick started.
    #[derive(Default)]
    struct RestoreAdmissionLog {
        /// Frames the terminal was asked to draw.
        draws: AtomicUsize,
        /// `draws` as of the start of the tick now running. The terminal
        /// republishes it when a tick ends, which is when it reads the next key.
        draws_at_tick_start: AtomicUsize,
        /// Inventory calls the admitted jobs have made. A job that has stopped
        /// calling is a job that has finished.
        calls: AtomicUsize,
        admissions: Mutex<Vec<RestoreAdmission>>,
    }

    impl RestoreAdmissionLog {
        fn drew(&self) {
            self.draws.fetch_add(1, Ordering::SeqCst);
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        /// Close the current tick. Called from the terminal's `read_key`, the
        /// last thing a loop iteration does.
        fn tick_ended(&self) {
            self.draws_at_tick_start
                .store(self.draws.load(Ordering::SeqCst), Ordering::SeqCst);
        }

        /// Record one inventory call. A job's first call is its admission, and
        /// every call is what the driver watches to know the job is still
        /// running.
        fn admitted(&self, job: std::thread::ThreadId) {
            let tick_start = self.draws_at_tick_start.load(Ordering::SeqCst);
            let drawn = self.draws.load(Ordering::SeqCst);
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut admissions = self.admitted_jobs();
            if admissions.last().map(|admission| admission.job) != Some(job) {
                admissions.push(RestoreAdmission {
                    job,
                    drawn,
                    skipped: drawn == tick_start,
                });
            }
        }

        /// A retry — an admission after the first — ran on a tick that drew
        /// nothing. This is the contract #554 has to keep, and the observation
        /// the loop is driven until it makes.
        fn retry_admitted_on_a_skipped_tick(&self) -> bool {
            self.admitted_jobs()
                .iter()
                .skip(1)
                .any(|admission| admission.skipped)
        }

        fn admitted_jobs(&self) -> std::sync::MutexGuard<'_, Vec<RestoreAdmission>> {
            self.admissions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    /// A restore client that always fails and records, once per admitted job,
    /// what the frame gate did on the tick that admitted it.
    struct AdmissionCountingRestorePort {
        log: Arc<RestoreAdmissionLog>,
    }

    impl AgentCommandPort for AdmissionCountingRestorePort {
        fn launch(
            &mut self,
            _operation: OperationId,
            _workspace: WorkspaceId,
            _session: Option<SessionId>,
            _profile: Option<AgentProfileId>,
        ) -> Result<AgentPaneAdmission, String> {
            Err("launch is unavailable".to_owned())
        }

        fn list_terminals(&mut self) -> Result<Vec<TerminalInventoryEntry>, TerminalError> {
            self.log.admitted(std::thread::current().id());
            Err(TerminalError::Unavailable)
        }
    }

    /// Ticks the retry acceptance drives before giving up. The retry it waits for
    /// comes due after one backoff step (250ms), so this is a wide bound whose
    /// only job is to end a run that observes nothing in its assertions instead
    /// of driving forever.
    const MAX_DRIVEN_RETRY_TICKS: usize = 200;

    /// Ticks without a new inventory call the driver waits for before quitting.
    /// The observed job is admitted on its first call and sleeps between its
    /// three attempts, so quiet ticks are how the driver knows it has finished.
    /// Returning from the loop mid-job would leave that worker running into the
    /// rest of the suite, where it keeps writing `spawn_restore_job`'s coverage
    /// counters while the harness reads them — enough to make a line another
    /// test covers report as uncovered. The next retry is a 500ms backoff step
    /// away, so this quiet window stays inside the gap and admits nothing new.
    const RETRY_QUIET_TICKS: usize = 2;

    /// A terminal that paces the loop so wall time advances between frames and
    /// keeps it running with an inert key until the admission log holds the
    /// observation the test needs, then quits. Driving to the observation is what
    /// makes the assertion deterministic: which tick skips its frame depends on
    /// when the material last changed, so no fixed key script can promise that a
    /// retry lands on a skipped tick (#567).
    struct RetryDrivingTerminal {
        log: Arc<RestoreAdmissionLog>,
        pace: std::time::Duration,
        /// Ticks driven so far, bounded by [`MAX_DRIVEN_RETRY_TICKS`].
        ticks: usize,
        /// Inventory calls seen at the previous tick, and how many ticks have
        /// passed without a new one.
        calls: usize,
        quiet_ticks: usize,
        /// The quit sequence, queued once the loop is done driving.
        quit: VecDeque<Key>,
    }

    impl Terminal for RetryDrivingTerminal {
        fn size(&mut self) -> io::Result<(usize, usize)> {
            Ok((20, 80))
        }

        fn draw(&mut self, _frame: &[String]) -> io::Result<()> {
            self.log.drew();
            Ok(())
        }

        fn wait(&mut self, _duration: std::time::Duration) -> io::Result<()> {
            Ok(())
        }

        fn read_key(&mut self) -> io::Result<Key> {
            // The last call of a loop iteration, so the frame count from here is
            // the one the next tick starts from.
            self.log.tick_ended();
            if let Some(key) = self.quit.pop_front() {
                return Ok(key);
            }
            let calls = self.log.calls();
            self.quiet_ticks = if calls == self.calls {
                self.quiet_ticks + 1
            } else {
                self.calls = calls;
                0
            };
            let settled = self.quiet_ticks >= RETRY_QUIET_TICKS
                && self.log.retry_admitted_on_a_skipped_tick();
            if settled || self.ticks >= MAX_DRIVEN_RETRY_TICKS {
                self.quit.push_back(Key::Char('y'));
                return Ok(Key::CtrlQ);
            }
            self.ticks += 1;
            // Wall time has to advance for the retry backoff to come due.
            std::thread::sleep(self.pace);
            // `Escape` is inert on the base Switch route, so driving with it
            // leaves the frame's material unchanged.
            Ok(Key::Escape)
        }
    }

    /// Park until the wall clock has just crossed into a new second.
    ///
    /// The frame material carries the wall clock truncated to seconds, so a
    /// second boundary redraws whatever the workspace is doing. A run that
    /// starts here and finishes inside the same second sees no clock-driven
    /// redraw, which is what makes the skipped tick it observes reproducible
    /// rather than a matter of which phase of the second the run began in
    /// (#567).
    fn wait_for_a_fresh_wall_clock_second() {
        let nanos = u64::from(Utc::now().nanosecond().min(999_999_999));
        std::thread::sleep(std::time::Duration::from_nanos(1_000_000_000 - nanos));
    }

    /// #554 acceptance. The skip covers the drawing and nothing else: the
    /// restore retry's admission sits after the gate and must still fire on a
    /// tick that drew nothing. The loop is driven until a retry is admitted on
    /// such a tick, so the assertion never rests on a run where every tick
    /// happened to be material (#567).
    #[test]
    fn a_skipped_tick_still_admits_the_restore_retry() {
        wait_for_a_fresh_wall_clock_second();
        let log = Arc::new(RestoreAdmissionLog::default());
        let mut term = RetryDrivingTerminal {
            log: Arc::clone(&log),
            pace: std::time::Duration::from_millis(60),
            ticks: 0,
            calls: 0,
            quiet_ticks: 0,
            quit: VecDeque::new(),
        };
        let mut factory = FixedBackendFactory {
            sessions: Some(Box::new(UnavailableSessionCommandPort)),
            agent: Some(Box::new(UnavailableAgentCommandPort)),
            launch: None,
            restore: Some(Box::new(AdmissionCountingRestorePort {
                log: Arc::clone(&log),
            })),
            metrics: Some(Box::new(NoMetrics)),
            browser: Some(Box::new(UnavailableBrowserOpener)),
            session_refresh: None,
            decisions: None,
            session_worktrees: None,
        };

        assert_eq!(
            run_workspace_controller_with_backend(&mut term, snapshot("retry"), &mut factory)
                .unwrap(),
            Exit::Quit
        );

        let admissions = log.admitted_jobs();
        let driven = term.ticks;
        let drawn_at: Vec<usize> = admissions.iter().map(|admission| admission.drawn).collect();
        let admitted = drawn_at.len();
        assert!(
            admitted >= 2,
            "the restore retry was admitted {admitted} time(s) in {driven} tick(s)"
        );
        // A retry admitted on a skipped tick is the whole contract, so a run that
        // never skipped one fails here instead of reading as a pass.
        let retry_on_a_skipped_tick = admissions.iter().skip(1).any(|admission| admission.skipped);
        assert!(
            retry_on_a_skipped_tick,
            "every restore retry followed a redraw in {driven} tick(s), \
             admitted at frame {drawn_at:?}"
        );
    }

    /// #554 acceptance. Skipping is decided by comparing the renderer's inputs,
    /// so this pins each of those inputs: change one and the frame must differ,
    /// change none and it must not — including across the ticks the rabbit
    /// spends resting.
    #[test]
    #[allow(clippy::too_many_lines)] // One arm per material the renderer reads; splitting hides the table.
    fn the_frame_material_changes_for_every_input_the_renderer_reads() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let record = SessionRecord {
            name: "alpha".to_owned(),
            display_name: None,
            origin: SessionOrigin::Human,
            started_from: None,
            root: PathBuf::from("/tmp/demo/alpha"),
            created_at: now(),
            last_active: None,
            notes: Scratchpad::default(),
            prs: Vec::new(),
        };
        let sessions = vec![ProjectedSession::from_record(session, &record)];
        let root = PathBuf::from("/tmp/demo");
        let no_diffs = BTreeMap::new();
        let clock = now();
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);

        let material = |runtime: &WorkspaceRuntime| {
            home_frame_material(
                20,
                80,
                runtime,
                "demo",
                &root,
                &sessions,
                None,
                health(),
                &no_diffs,
                None,
                None,
                clock,
            )
        };
        let base = material(&runtime);

        // A resting tick is not material: the rabbit's four idle phases collapse
        // onto one frame, so the loop draws nothing while it holds its pose.
        for _ in 0..3 {
            let _ = runtime.apply_event(AppEvent::Tick);
            assert_eq!(material(&runtime), base, "a resting tick forced a redraw");
        }
        // The blink and the ear flop are: both must reach the terminal.
        let _ = runtime.apply_event(AppEvent::Tick);
        let blink = material(&runtime);
        assert_ne!(blink, base, "the rabbit stopped blinking");
        let _ = runtime.apply_event(AppEvent::Tick);
        let flop = material(&runtime);
        assert_ne!(flop, blink, "the rabbit stopped flopping its ear");
        let _ = runtime.apply_event(AppEvent::Tick);
        assert_eq!(
            material(&runtime),
            base,
            "the rabbit never came back to rest"
        );

        // Terminal size (a resize that actually changes the geometry).
        let resized = home_frame_material(
            21,
            80,
            &runtime,
            "demo",
            &root,
            &sessions,
            None,
            health(),
            &no_diffs,
            None,
            None,
            clock,
        );
        assert_ne!(resized, base, "a resize did not redraw");

        // The wall clock behind the sidebar's relative session times. It is
        // material at whole-second resolution: sub-second jitter must not force
        // a redraw, but a new second must.
        let sub_second = home_frame_material(
            20,
            80,
            &runtime,
            "demo",
            &root,
            &sessions,
            None,
            health(),
            &no_diffs,
            None,
            None,
            clock + Duration::milliseconds(400),
        );
        assert_eq!(sub_second, base, "sub-second jitter forced a redraw");
        let next_second = home_frame_material(
            20,
            80,
            &runtime,
            "demo",
            &root,
            &sessions,
            None,
            health(),
            &no_diffs,
            None,
            None,
            clock + Duration::seconds(1),
        );
        assert_ne!(next_second, base, "the relative session times froze");

        // Daemon metrics for the mascot sidecar.
        let metrics = home_frame_material(
            20,
            80,
            &runtime,
            "demo",
            &root,
            &sessions,
            StaticMetrics.latest(),
            health(),
            &no_diffs,
            None,
            None,
            clock,
        );
        assert_ne!(metrics, base, "a metrics update did not redraw");

        // The diagnostic health observer. It is a renderer input, so it belongs
        // to the material: a newly observed sample must be able to change the
        // sidecar, and an unchanged observer must not force a redraw.
        let mut observed = DaemonHealthTracker::default();
        observed.observe(&StaticMetrics.latest().expect("static metrics"));
        let health_material = home_frame_material(
            20, 80, &runtime, "demo", &root, &sessions, None, observed, &no_diffs, None, None,
            clock,
        );
        assert_ne!(health_material, base, "a health observation did not redraw");

        // Git diffs joined onto the sidebar rows.
        let diffs = BTreeMap::from([(
            session,
            GitDiff {
                base: "main".to_owned(),
                ahead: 1,
                behind: 0,
                added: 1,
                removed: 2,
            },
        )]);
        let git = home_frame_material(
            20,
            80,
            &runtime,
            "demo",
            &root,
            &sessions,
            None,
            health(),
            &diffs,
            None,
            None,
            clock,
        );
        assert_ne!(git, base, "a git diff update did not redraw");

        // Live terminal output.
        let view = TerminalViewProjection {
            rows: vec!["output".to_owned()],
            row_offset: 0,
            total_rows: 1,
            scroll: 0,
            feedback: None,
        };
        let terminal_output = home_frame_material(
            20,
            80,
            &runtime,
            "demo",
            &root,
            &sessions,
            None,
            health(),
            &no_diffs,
            Some(view),
            None,
            clock,
        );
        assert_ne!(terminal_output, base, "terminal output did not redraw");

        // The pending create skeleton.
        let pending = home_frame_material(
            20,
            80,
            &runtime,
            "demo",
            &root,
            &sessions,
            None,
            health(),
            &no_diffs,
            None,
            Some("beta"),
            clock,
        );
        assert_ne!(pending, base, "a pending create did not redraw");

        // Reducer state, and the two overlays composited outside `render_home`.
        let mut moved = WorkspaceRuntime::new(workspace, vec![session]);
        let _ = moved.handle_key(Key::Down);
        assert_ne!(material(&moved), base, "a selection move did not redraw");

        let mut quitting = WorkspaceRuntime::new(workspace, vec![session]);
        let _ = quitting.handle_key(Key::CtrlQ);
        let confirming = material(&quitting);
        assert_ne!(confirming, base, "the quit confirmation did not redraw");
        let _ = quitting.handle_key(Key::Left);
        assert_ne!(
            material(&quitting),
            confirming,
            "moving the quit confirmation's focus did not redraw"
        );
    }

    /// #554 acceptance for the entry screens. They have no clock and no
    /// background lane, so an idle Welcome must draw exactly once however long
    /// the terminal keeps ticking.
    #[test]
    fn idle_entry_screen_ticks_draw_nothing_after_the_first_frame() {
        let mut keys = vec![Key::Other; 60];
        keys.push(Key::Char('q'));
        let mut term = FakeTerminal::with_keys(&keys);
        let mut loader = FakeLoader::default();
        let mut settings = DefaultSettingsPort;
        let mut sessions = UnavailableSessionCommandPortFactory;

        assert_eq!(
            run_with_settings(
                &mut term,
                Vec::new(),
                Vec::new(),
                now(),
                Start::Welcome,
                &mut loader,
                &mut settings,
                &mut sessions,
            )
            .unwrap(),
            Exit::Quit
        );

        assert_eq!(
            term.frames.len(),
            1,
            "an idle Welcome rebuilt its frame on a tick"
        );
    }

    /// The entry gate still redraws the moment the form or the screen changes,
    /// so input latency is unaffected: every tick that carries a change draws,
    /// and only the empty ones in between are skipped.
    #[test]
    fn entry_screen_input_redraws_immediately() {
        let mut term = FakeTerminal::with_keys(&[
            Key::Other,
            Key::Down,
            Key::Other,
            Key::Enter,
            Key::Other,
            Key::Escape,
            Key::Other,
            Key::Char('q'),
        ]);
        let mut loader = FakeLoader::default();
        let mut settings = DefaultSettingsPort;
        let mut sessions = UnavailableSessionCommandPortFactory;

        assert_eq!(
            run_with_settings(
                &mut term,
                Vec::new(),
                Vec::new(),
                now(),
                Start::Welcome,
                &mut loader,
                &mut settings,
                &mut sessions,
            )
            .unwrap(),
            Exit::Quit
        );

        // Welcome, the selection move, the screen it opens, and Welcome again:
        // one frame per input that changed something, none for the four
        // interleaved ticks.
        assert_eq!(term.frames.len(), 4);
    }

    /// #551 acceptance: several `RefreshSessions` inside one cadence period are
    /// answered by the one snapshot the lane publishes, and a lane that never
    /// answers parks at most one completion instead of accumulating them.
    #[test]
    fn refresh_requests_coalesce_onto_one_published_snapshot() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let mut pending_targets = std::collections::HashMap::new();
        let mut pending_refresh = None;
        let (host, actions) = ControllerHost::channel();
        let mut backend = DaemonBackend::new(
            Box::new(host.clone()),
            Box::new(host),
            Box::new(UnavailableBackendPort),
            Box::new(UnavailableBackendPort),
        );
        let wakes = Arc::new(AtomicUsize::new(0));
        let published = SessionId::new();
        let mut lane = FakeSessionRefreshPort {
            wakes: Arc::clone(&wakes),
            takes: Arc::default(),
            queued: Arc::new(Mutex::new(VecDeque::from([Ok(SessionCommandResult {
                message: "daemon snapshot refreshed".to_owned(),
                sessions: Some(ui.workspace.sessions().to_vec()),
                session_ids: Some(vec![published]),
                agent_resumes: None,
                session_lifecycles: None,
                session_roles: None,
                revision: Some(7),
            })]))),
        };

        for _ in 0..3 {
            backend.dispatch(Effect::RefreshSessions { workspace });
        }
        super::drain_controller_host_actions(
            &actions,
            &mut ui,
            &mut runtime,
            &mut pending_targets,
            &mut lane,
            &mut pending_refresh,
        );
        assert_eq!(wakes.load(Ordering::SeqCst), 3);
        assert!(pending_refresh.is_some());

        super::drain_session_refresh(&mut ui, &mut lane, &mut pending_refresh);
        assert!(pending_refresh.is_none());
        assert_eq!(ui.workspace.session_ids(), &[published]);
        assert_eq!(ui.last_session_revision, 7);
        let events = backend.drain_events();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            AppEvent::Backend(BackendEvent::Sessions(ids)) if ids == &[published]
        ));

        // A second drain with nothing published leaves the frame untouched.
        super::drain_session_refresh(&mut ui, &mut lane, &mut pending_refresh);
        assert!(backend.drain_events().is_empty());

        // A legacy port that answers with rows but no stable identities leaves
        // the adopted ids alone, and the completion still reports the set Home
        // currently holds rather than an empty one.
        let (completions, events) =
            crate::usecase::application::daemon_backend::Completions::channel();
        pending_refresh = Some(completions);
        lane.queued
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(Ok(SessionCommandResult {
                message: "legacy snapshot".to_owned(),
                sessions: Some(ui.workspace.sessions().to_vec()),
                session_ids: None,
                agent_resumes: None,
                session_lifecycles: None,
                session_roles: None,
                revision: Some(8),
            }));
        super::drain_session_refresh(&mut ui, &mut lane, &mut pending_refresh);
        assert!(matches!(
            events.try_recv().unwrap(),
            AppEvent::Backend(BackendEvent::Sessions(ids)) if ids == vec![published]
        ));
    }

    /// A lane that fails reports it once through the parked completion and
    /// leaves the adopted snapshot alone, and a snapshot older than one already
    /// adopted is discarded whichever lane observed it.
    #[test]
    fn a_failed_or_stale_lane_observation_never_rewrites_the_adopted_snapshot() {
        let session = SessionId::new();
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
        ui.last_session_revision = 9;
        let (completions, events) =
            crate::usecase::application::daemon_backend::Completions::channel();
        let mut pending_refresh = Some(completions);
        let stale = SessionId::new();
        let mut lane = FakeSessionRefreshPort {
            wakes: Arc::default(),
            takes: Arc::default(),
            queued: Arc::new(Mutex::new(VecDeque::from([
                Err("daemon unavailable\ninternal detail".to_owned()),
                Ok(SessionCommandResult {
                    message: "stale".to_owned(),
                    sessions: Some(Vec::new()),
                    session_ids: Some(vec![stale]),
                    agent_resumes: None,
                    session_lifecycles: None,
                    session_roles: None,
                    revision: Some(3),
                }),
                Err("later daemon failure".to_owned()),
            ]))),
        };

        super::drain_session_refresh(&mut ui, &mut lane, &mut pending_refresh);
        assert!(pending_refresh.is_none());
        assert!(matches!(
            events.try_recv().unwrap(),
            AppEvent::Backend(BackendEvent::Notice(notice))
                if notice.message == "daemon unavailable"
        ));
        assert_eq!(ui.workspace.session_ids(), &[session]);

        super::drain_session_refresh(&mut ui, &mut lane, &mut pending_refresh);
        assert_eq!(ui.workspace.session_ids(), &[session]);
        assert_eq!(ui.last_session_revision, 9);

        // A lane error without a parked reducer completion is intentionally
        // consumed without synthesizing an event.
        super::drain_session_refresh(&mut ui, &mut lane, &mut pending_refresh);
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn direct_controller_entry_binds_workspace_config_settings() {
        let mut keys = vec![Key::Char(':')];
        keys.extend("config".chars().map(Key::Char));
        keys.extend([
            Key::Enter,
            Key::Quit,
            Key::CtrlQ,
            Key::Escape,
            Key::CtrlQ,
            Key::Char('y'),
        ]);
        let mut term = FakeTerminal::with_keys(&keys);
        let mut factory = FixedBackendFactory {
            sessions: Some(Box::new(UnavailableSessionCommandPort)),
            agent: Some(Box::new(UnavailableAgentCommandPort)),
            launch: None,
            restore: None,
            metrics: Some(Box::new(NoMetrics)),
            browser: Some(Box::new(UnavailableBrowserOpener)),
            session_refresh: None,
            decisions: None,
            session_worktrees: None,
        };
        let mut settings = WorkspaceBindingSettingsPort::default();

        assert_eq!(
            run_workspace_controller_with_backend_and_config(
                &mut term,
                snapshot("direct-config"),
                &mut factory,
                &mut settings,
                AvailableAgentModels::all(),
            )
            .unwrap(),
            Exit::Quit
        );
        assert_eq!(settings.selected, vec![PathBuf::from("/tmp/direct-config")]);
        assert!(term.frames.iter().any(|frame| {
            let frame = frame.join("\n");
            frame.contains("Config")
                && frame.contains("Agent")
                && !frame.contains("Scope:")
                && frame.contains("direct-config")
        }));
    }

    #[test]
    fn controller_loop_opens_the_create_form_from_the_new_session_row() {
        // An empty workspace shows only root and `+ new session`, so one Down
        // reaches the create entry deterministically.
        let snapshot = WorkspaceSnapshot::new(
            ws("empty"),
            WorkspaceState {
                sessions: Vec::new(),
                root_notes: Scratchpad::default(),
                updated_at: now(),
            },
        );
        let terminal = TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: snapshot.workspace_id,
            session_id: None,
            worktree_id: WorktreeId::new(),
        };
        // Down → + new session, Enter opens the create form, type a name, Esc
        // closes it, then Ctrl-Q + y detaches.
        let keys = [
            Key::Down,
            Key::Enter,
            Key::Char('a'),
            Key::Char('p'),
            Key::Char('i'),
            Key::Escape,
            Key::CtrlQ,
            Key::Char('y'),
        ];
        let mut term = FakeTerminal::with_keys(&keys);
        let result = run_workspace_controller(
            &mut term,
            snapshot,
            Box::new(UnavailableSessionCommandPort),
            Box::new(SuccessfulAgentPort(terminal.clone())),
            launch_port(Box::new(SuccessfulAgentPort(terminal))),
            Box::new(UnavailableDecisionCommandPort),
            Box::new(UnavailableEnvironmentStore),
            Box::new(NoDesktopNotifications),
            Box::new(NoMetrics),
            Box::new(UnavailablePrSnapshotPort),
            Box::new(UnavailableBrowserOpener),
        );

        assert!(matches!(result, Ok(Exit::Quit)));
        // The inline `+ new session` row rendered the typed name, confirming the
        // create-entry seam works through the controller loop. It is inline in the
        // sidebar, not a centered modal, so the old "New session" modal title never
        // appears.
        assert!(
            term.frames
                .iter()
                .any(|frame| frame.join("\n").contains("api"))
        );
        assert!(
            term.frames
                .iter()
                .all(|frame| !frame.join("\n").contains("New session"))
        );
    }

    #[test]
    fn controller_loop_dispatches_each_ctrl_a_representation_once_to_the_session_port() {
        struct SignallingSessionPort {
            calls: Arc<AtomicUsize>,
            create_call: std::sync::mpsc::Sender<String>,
        }

        impl SessionCommandPort for SignallingSessionPort {
            fn execute(
                &self,
                _: &Workspace,
                _: Option<&SessionRecord>,
                command: SessionCommand,
            ) -> Result<SessionCommandResult, String> {
                let SessionCommand::Create { name, .. } = command else {
                    return Err("unexpected session command".to_owned());
                };
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.create_call
                    .send(name)
                    .map_err(|error| error.to_string())?;
                Ok(SessionCommandResult::message("daemon accepted"))
            }
        }

        // The composition adapter normalizes a modified Ctrl+A to LineStart,
        // preserves a raw control byte as U+0001, and carries Home as Home. All
        // three must enter the same controller form and lifecycle dispatch path.
        for create_key in [Key::LineStart, Key::Char('\u{1}'), Key::Home] {
            let snapshot = WorkspaceSnapshot::new(
                ws("empty"),
                WorkspaceState {
                    sessions: Vec::new(),
                    root_notes: Scratchpad::default(),
                    updated_at: now(),
                },
            );
            let terminal = TerminalRef {
                daemon_generation: DaemonGeneration::new(),
                terminal_id: TerminalId::new(),
                workspace_id: snapshot.workspace_id,
                session_id: None,
                worktree_id: WorktreeId::new(),
            };
            let calls = Arc::new(AtomicUsize::new(0));
            let (create_call, observed_create) = std::sync::mpsc::channel();
            let keys = [
                create_key.clone(),
                Key::Char('a'),
                Key::Char('p'),
                Key::Char('i'),
                Key::Enter,
                Key::CtrlQ,
                Key::Char('y'),
            ];
            let mut term = FakeTerminal::with_keys_waiting_for_create(&keys, observed_create);

            let result = run_workspace_controller(
                &mut term,
                snapshot,
                Box::new(SignallingSessionPort {
                    calls: calls.clone(),
                    create_call,
                }),
                Box::new(SuccessfulAgentPort(terminal.clone())),
                launch_port(Box::new(SuccessfulAgentPort(terminal))),
                Box::new(UnavailableDecisionCommandPort),
                Box::new(UnavailableEnvironmentStore),
                Box::new(NoDesktopNotifications),
                Box::new(NoMetrics),
                Box::new(UnavailablePrSnapshotPort),
                Box::new(UnavailableBrowserOpener),
            );

            assert!(matches!(result, Ok(Exit::Quit)), "{create_key:?}");
            assert_eq!(calls.load(Ordering::SeqCst), 1, "{create_key:?}");
            assert_eq!(term.observed_creates, ["api"], "{create_key:?}");
        }
    }

    #[test]
    fn drain_session_completions_refluxes_create_failure_with_its_token() {
        let snapshot = snapshot("demo");
        let view = WorkspaceView::with_runtime_ids(
            snapshot.workspace,
            snapshot.state,
            snapshot.session_ids,
        );
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
        let token = PendingToken::from_raw(41);

        // A create worker returned a display-safe daemon rejection (e.g. a name the
        // daemon refuses). The legacy path used to drop this on the floor; it must
        // now reflux as a controller notice so the user sees the failure.
        let (backend_completions, backend_receiver) =
            crate::usecase::application::daemon_backend::Completions::channel();
        let result = Err("daemon refused the session".to_owned());
        let completion = super::SessionBackendCompletion::Create {
            token,
            before: Vec::new(),
            completions: backend_completions,
        };
        super::emit_session_command_result(&result, &completion);
        ui.active_session_command = Some(1);
        ui.session_completion_sender
            .send(super::SessionCommandCompletion {
                command_id: 1,
                result,
                completion,
            })
            .unwrap();

        super::drain_session_completions(&mut ui);
        assert!(matches!(
            backend_receiver.recv().unwrap(),
            AppEvent::OperationResult(result)
                if result.token == token
                    && !result.succeeded
                    && result.created.is_none()
                    && result.notice.as_ref().is_some_and(|notice| notice.message == "daemon refused the session")
        ));
    }

    #[test]
    fn session_commands_reject_the_second_request_as_busy() {
        let snapshot = snapshot("demo");
        let view = WorkspaceView::with_runtime_ids(
            snapshot.workspace,
            snapshot.state,
            snapshot.session_ids,
        );
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
        let (first_completions, _) =
            crate::usecase::application::daemon_backend::Completions::channel();
        let (second_completions, _) =
            crate::usecase::application::daemon_backend::Completions::channel();

        assert!(super::begin_session_command(
            &mut ui,
            SessionCommand::List,
            super::SessionBackendCompletion::Remove {
                session: SessionId::new(),
                before: Vec::new(),
                completions: first_completions,
            },
        ));
        assert!(!super::begin_session_command(
            &mut ui,
            SessionCommand::List,
            super::SessionBackendCompletion::Remove {
                session: SessionId::new(),
                before: Vec::new(),
                completions: second_completions,
            },
        ));
    }

    #[test]
    fn stale_session_completion_does_not_replace_a_newer_snapshot() {
        let snapshot = snapshot("demo");
        let original = snapshot.session_ids[0];
        let view = WorkspaceView::with_runtime_ids(
            snapshot.workspace,
            snapshot.state,
            snapshot.session_ids,
        );
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
        let (newer_completions, _) =
            crate::usecase::application::daemon_backend::Completions::channel();
        let (older_completions, _) =
            crate::usecase::application::daemon_backend::Completions::channel();
        let newer = SessionId::new();
        let mut newer_record = ui.workspace.sessions()[0].clone();
        newer_record.name = "newer".to_owned();

        ui.active_session_command = Some(2);
        ui.session_completion_sender
            .send(super::SessionCommandCompletion {
                command_id: 2,
                result: Ok(SessionCommandResult {
                    message: "newer".to_owned(),
                    sessions: Some(vec![newer_record]),
                    session_ids: Some(vec![newer]),
                    agent_resumes: None,
                    session_lifecycles: None,
                    session_roles: None,
                    revision: Some(2),
                }),
                completion: super::SessionBackendCompletion::Remove {
                    session: SessionId::new(),
                    before: vec![original],
                    completions: newer_completions,
                },
            })
            .unwrap();
        super::drain_session_completions(&mut ui);

        ui.active_session_command = Some(1);
        ui.session_completion_sender
            .send(super::SessionCommandCompletion {
                command_id: 1,
                result: Ok(SessionCommandResult {
                    message: "older".to_owned(),
                    sessions: Some(ui.workspace.sessions().to_vec()),
                    session_ids: Some(vec![original]),
                    agent_resumes: None,
                    session_lifecycles: None,
                    session_roles: None,
                    revision: Some(1),
                }),
                completion: super::SessionBackendCompletion::Remove {
                    session: SessionId::new(),
                    before: vec![newer],
                    completions: older_completions,
                },
            })
            .unwrap();

        super::drain_session_completions(&mut ui);
        assert_eq!(ui.workspace.session_ids(), &[newer]);
        assert_eq!(ui.workspace.sessions()[0].name, "newer");
    }

    #[test]
    fn drain_session_completions_refluxes_create_success_with_created_identity() {
        let snapshot = snapshot("demo");
        let existing = snapshot.session_ids[0];
        let created = SessionId::new();
        let mut records = snapshot.state.sessions.clone();
        let mut new_record = records[0].clone();
        new_record.name = "created".to_owned();
        records.push(new_record);
        let view = WorkspaceView::with_runtime_ids(
            snapshot.workspace,
            snapshot.state,
            snapshot.session_ids,
        );
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
        let token = PendingToken::from_raw(42);
        let (completions, receiver) =
            crate::usecase::application::daemon_backend::Completions::channel();
        let result = Ok(SessionCommandResult {
            message: "created".to_owned(),
            sessions: Some(records),
            session_ids: Some(vec![existing, created]),
            agent_resumes: None,
            session_lifecycles: None,
            session_roles: None,
            revision: None,
        });
        let completion = super::SessionBackendCompletion::Create {
            token,
            before: vec![existing],
            completions,
        };
        super::emit_session_command_result(&result, &completion);
        ui.active_session_command = Some(1);

        ui.session_completion_sender
            .send(super::SessionCommandCompletion {
                command_id: 1,
                result,
                completion,
            })
            .unwrap();
        super::drain_session_completions(&mut ui);

        assert!(matches!(
            receiver.recv().unwrap(),
            AppEvent::OperationResult(result)
                if result.token == token && result.succeeded && result.created == Some(created)
        ));
    }

    #[test]
    fn session_snapshot_completion_preserves_fallback_and_reports_failure_once() {
        let existing = SessionId::new();
        let (completions, receiver) =
            crate::usecase::application::daemon_backend::Completions::channel();
        let completion = super::SessionBackendCompletion::Remove {
            session: SessionId::new(),
            before: vec![existing],
            completions,
        };
        super::emit_session_command_result(
            &Ok(SessionCommandResult::message("legacy snapshot")),
            &completion,
        );
        assert!(matches!(
            receiver.recv().unwrap(),
            AppEvent::Backend(BackendEvent::Sessions(sessions)) if sessions == [existing]
        ));
        assert!(receiver.try_recv().is_err());

        let (completions, receiver) =
            crate::usecase::application::daemon_backend::Completions::channel();
        let completion = super::SessionBackendCompletion::Remove {
            session: SessionId::new(),
            before: vec![existing],
            completions,
        };
        super::emit_session_command_result(&Err("daemon unavailable".to_owned()), &completion);
        assert!(matches!(
            receiver.recv().unwrap(),
            AppEvent::Backend(BackendEvent::Notice(notice)) if notice.message == "daemon unavailable"
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[derive(Clone, Copy)]
    enum ConcurrentSessionRequest {
        Create(u64),
        Remove,
    }

    struct BlockingSessionPort {
        existing: SessionId,
        created: SessionId,
        calls: Arc<Mutex<Vec<SessionCommand>>>,
        started: std::sync::mpsc::Sender<()>,
        release: Mutex<Receiver<()>>,
        block_once: AtomicBool,
    }

    impl SessionCommandPort for BlockingSessionPort {
        fn execute(
            &self,
            _: &Workspace,
            _: Option<&SessionRecord>,
            command: SessionCommand,
        ) -> Result<SessionCommandResult, String> {
            self.calls.lock().unwrap().push(command.clone());
            if self.block_once.swap(false, Ordering::SeqCst) {
                let _ = self.started.send(());
                let _ = self.release.lock().unwrap().recv();
            }
            let session_ids = match command {
                SessionCommand::Create { .. } => {
                    vec![self.existing, self.created]
                }
                SessionCommand::Remove { .. } => Vec::new(),
                _ => vec![self.existing],
            };
            Ok(SessionCommandResult {
                message: "completed".to_owned(),
                sessions: None,
                session_ids: Some(session_ids),
                agent_resumes: None,
                session_lifecycles: None,
                session_roles: None,
                revision: None,
            })
        }
    }

    fn enqueue_session_request(
        host: &mut ControllerHost,
        request: ConcurrentSessionRequest,
        workspace: WorkspaceId,
        session: SessionId,
    ) -> Receiver<AppEvent> {
        use crate::usecase::application::daemon_backend::SessionCommandPort as _;

        let (completions, receiver) =
            crate::usecase::application::daemon_backend::Completions::channel();
        match request {
            ConcurrentSessionRequest::Create(token) => host.create(
                crate::usecase::application::daemon_backend::CreateSessionRequest {
                    workspace,
                    token: PendingToken::from_raw(token),
                    operation_id: OperationId::new(),
                    intent: SessionCreateIntent {
                        name: format!("session-{token}"),
                        profile: None,
                        model: None,
                        role_id: None,
                    },
                },
                completions,
            ),
            ConcurrentSessionRequest::Remove => host.remove(
                crate::usecase::application::daemon_backend::RemoveSessionRequest {
                    workspace,
                    session,
                    force: false,
                    force_delete_branch: false,
                },
                completions,
            ),
        }
        receiver
    }

    fn assert_busy_pair(first: ConcurrentSessionRequest, second: ConcurrentSessionRequest) {
        let snapshot = snapshot("demo");
        let workspace = snapshot.workspace_id;
        let session = snapshot.session_ids[0];
        let created = SessionId::new();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let view = WorkspaceView::with_runtime_ids(
            snapshot.workspace,
            snapshot.state,
            snapshot.session_ids,
        );
        let mut ui = WorkspaceUi::new(
            view,
            Box::new(BlockingSessionPort {
                existing: session,
                created,
                calls: calls.clone(),
                started: started_tx,
                release: Mutex::new(release_rx),
                block_once: AtomicBool::new(true),
            }),
        );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let (mut host, actions) = ControllerHost::channel();
        let first_completion = enqueue_session_request(&mut host, first, workspace, session);
        drain_host_actions(
            &actions,
            &mut ui,
            &mut runtime,
            &mut std::collections::HashMap::new(),
        );
        started_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();

        let second_completion = enqueue_session_request(&mut host, second, workspace, session);
        drain_host_actions(
            &actions,
            &mut ui,
            &mut runtime,
            &mut std::collections::HashMap::new(),
        );
        let busy = second_completion
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(match busy {
            AppEvent::OperationResult(result) => {
                !result.succeeded
                    && result.notice.is_some_and(|notice| {
                        notice.message == "session command is already running"
                    })
            }
            AppEvent::Backend(BackendEvent::Notice(notice)) => {
                notice.message == "session command is already running"
            }
            _ => false,
        });
        assert!(second_completion.try_recv().is_err());

        release_tx.send(()).unwrap();
        first_completion
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(first_completion.try_recv().is_err());
        for _ in 0..100 {
            drain_session_completions(&mut ui);
            if ui.active_session_command.is_none() {
                break;
            }
            std::thread::yield_now();
        }
        assert!(ui.active_session_command.is_none());
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn concurrent_create_create_completes_second_as_busy() {
        assert_busy_pair(
            ConcurrentSessionRequest::Create(1),
            ConcurrentSessionRequest::Create(2),
        );
    }

    #[test]
    fn concurrent_create_remove_completes_second_as_busy() {
        assert_busy_pair(
            ConcurrentSessionRequest::Create(1),
            ConcurrentSessionRequest::Remove,
        );
    }

    #[test]
    fn concurrent_remove_create_completes_second_as_busy() {
        assert_busy_pair(
            ConcurrentSessionRequest::Remove,
            ConcurrentSessionRequest::Create(2),
        );
    }

    struct PanicOnceSessionPort {
        existing: SessionId,
        created: SessionId,
        panics: AtomicBool,
    }

    impl SessionCommandPort for PanicOnceSessionPort {
        fn execute(
            &self,
            _: &Workspace,
            _: Option<&SessionRecord>,
            _: SessionCommand,
        ) -> Result<SessionCommandResult, String> {
            assert!(
                !self.panics.swap(false, Ordering::SeqCst),
                "fake session worker panic"
            );
            Ok(SessionCommandResult {
                message: "recovered".to_owned(),
                sessions: None,
                session_ids: Some(vec![self.existing, self.created]),
                agent_resumes: None,
                session_lifecycles: None,
                session_roles: None,
                revision: None,
            })
        }
    }

    #[test]
    fn session_worker_panic_completes_and_returns_the_port() {
        let snapshot = snapshot("demo");
        let workspace = snapshot.workspace_id;
        let session = snapshot.session_ids[0];
        let view = WorkspaceView::with_runtime_ids(
            snapshot.workspace,
            snapshot.state,
            snapshot.session_ids,
        );
        let mut ui = WorkspaceUi::new(
            view,
            Box::new(PanicOnceSessionPort {
                existing: session,
                created: SessionId::new(),
                panics: AtomicBool::new(true),
            }),
        );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let (mut host, actions) = ControllerHost::channel();
        let failed = enqueue_session_request(
            &mut host,
            ConcurrentSessionRequest::Create(1),
            workspace,
            session,
        );
        drain_host_actions(
            &actions,
            &mut ui,
            &mut runtime,
            &mut std::collections::HashMap::new(),
        );
        assert!(matches!(
            failed
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap(),
            AppEvent::OperationResult(result)
                if !result.succeeded
                    && result.notice.as_ref().is_some_and(|notice| notice.message == "session command worker failed")
        ));
        for _ in 0..100 {
            drain_session_completions(&mut ui);
            if ui.active_session_command.is_none() {
                break;
            }
            std::thread::yield_now();
        }
        assert!(ui.active_session_command.is_none());

        let recovered = enqueue_session_request(
            &mut host,
            ConcurrentSessionRequest::Create(2),
            workspace,
            session,
        );
        drain_host_actions(
            &actions,
            &mut ui,
            &mut runtime,
            &mut std::collections::HashMap::new(),
        );
        assert!(matches!(
            recovered
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap(),
            AppEvent::OperationResult(result) if result.succeeded
        ));
    }

    #[test]
    fn closed_session_host_channel_completes_each_effect_once() {
        use crate::usecase::application::daemon_backend::SessionCommandPort as _;

        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let (mut host, actions) = ControllerHost::channel();
        drop(actions);

        for request in [
            ConcurrentSessionRequest::Create(1),
            ConcurrentSessionRequest::Remove,
        ] {
            let completion = enqueue_session_request(&mut host, request, workspace, session);
            assert!(matches!(
                completion
                    .recv_timeout(std::time::Duration::from_secs(1))
                    .unwrap(),
                AppEvent::OperationResult(_) | AppEvent::Backend(BackendEvent::Notice(_))
            ));
            assert!(completion.try_recv().is_err());
        }

        let (completions, completion) =
            crate::usecase::application::daemon_backend::Completions::channel();
        host.refresh(workspace, completions);
        assert!(matches!(
            completion
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap(),
            AppEvent::Backend(BackendEvent::Notice(_))
        ));
        assert!(completion.try_recv().is_err());
    }

    #[test]
    fn out_of_order_session_completion_cannot_release_the_active_port() {
        let snapshot = snapshot("demo");
        let view = WorkspaceView::with_runtime_ids(
            snapshot.workspace,
            snapshot.state,
            snapshot.session_ids,
        );
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
        ui.active_session_command = Some(2);
        let result = Ok(SessionCommandResult::message("done"));
        let (completions, _) = crate::usecase::application::daemon_backend::Completions::channel();

        ui.session_completion_sender
            .send(super::SessionCommandCompletion {
                command_id: 1,
                result: result.clone(),
                completion: super::SessionBackendCompletion::Remove {
                    session: SessionId::new(),
                    before: Vec::new(),
                    completions,
                },
            })
            .unwrap();
        drain_session_completions(&mut ui);
        assert_eq!(ui.active_session_command, Some(2));

        let (completions, _) = crate::usecase::application::daemon_backend::Completions::channel();
        ui.session_completion_sender
            .send(super::SessionCommandCompletion {
                command_id: 2,
                result,
                completion: super::SessionBackendCompletion::Remove {
                    session: SessionId::new(),
                    before: Vec::new(),
                    completions,
                },
            })
            .unwrap();
        drain_session_completions(&mut ui);
        assert_eq!(ui.active_session_command, None);
    }

    #[test]
    fn workspace_exit_does_not_drop_the_admitted_effect_completion() {
        let snapshot = snapshot("demo");
        let workspace = snapshot.workspace_id;
        let session = snapshot.session_ids[0];
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let view = WorkspaceView::with_runtime_ids(
            snapshot.workspace,
            snapshot.state,
            snapshot.session_ids,
        );
        let mut ui = WorkspaceUi::new(
            view,
            Box::new(BlockingSessionPort {
                existing: session,
                created: SessionId::new(),
                calls: Arc::new(Mutex::new(Vec::new())),
                started: started_tx,
                release: Mutex::new(release_rx),
                block_once: AtomicBool::new(true),
            }),
        );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let (mut host, actions) = ControllerHost::channel();
        let completion = enqueue_session_request(
            &mut host,
            ConcurrentSessionRequest::Create(1),
            workspace,
            session,
        );
        drain_host_actions(
            &actions,
            &mut ui,
            &mut runtime,
            &mut std::collections::HashMap::new(),
        );
        started_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        drop(ui);
        drop(runtime);
        drop(actions);
        release_tx.send(()).unwrap();

        assert!(matches!(
            completion
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap(),
            AppEvent::OperationResult(_)
        ));
        assert!(completion.try_recv().is_err());
    }

    #[test]
    fn session_snapshot_adapter_preserves_reconciliation_boundary_for_pointer_state() {
        use crate::presentation::workspace_runtime::WorkspaceRuntime;
        use crate::usecase::application::controller::{HomeMode, Route};

        let snapshot = snapshot("demo");
        let workspace_id = snapshot.workspace_id;
        let session = snapshot.session_ids[0];
        let records = snapshot.state.sessions.clone();
        let view = WorkspaceView::with_runtime_ids(
            snapshot.workspace,
            snapshot.state,
            snapshot.session_ids,
        );
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
        let mut runtime = WorkspaceRuntime::new(workspace_id, vec![session]);
        let _ = runtime.apply_event(AppEvent::Resize {
            width: 100,
            height: 30,
        });
        let _ = runtime.apply_event(sidebar_pointer_event(
            5,
            2,
            std::time::Duration::from_millis(1_000),
        ));

        let (completions, receiver) =
            crate::usecase::application::daemon_backend::Completions::channel();
        let result = Ok(SessionCommandResult {
            message: "same snapshot".to_owned(),
            sessions: Some(records),
            session_ids: Some(vec![session]),
            agent_resumes: None,
            session_lifecycles: None,
            session_roles: None,
            revision: None,
        });
        let completion = super::SessionBackendCompletion::Remove {
            session: SessionId::new(),
            before: vec![session],
            completions,
        };
        super::emit_session_command_result(&result, &completion);
        ui.active_session_command = Some(1);
        ui.session_completion_sender
            .send(super::SessionCommandCompletion {
                command_id: 1,
                result,
                completion,
            })
            .unwrap();
        super::drain_session_completions(&mut ui);
        let _ = runtime.apply_event(receiver.recv().unwrap());
        assert_eq!(runtime.state().sessions(), &[session]);
        let _ = runtime.apply_event(sidebar_pointer_event(
            5,
            2,
            std::time::Duration::from_millis(1_100),
        ));

        let _ = workspace_id;
        assert_eq!(runtime.state().active(), Some(session));
        assert!(matches!(
            runtime.state().route(),
            Route::Home(HomeMode::Switch)
        ));
    }

    /// The attach payload a daemon at `geometry` returns after producing
    /// `bytes`: the daemon is the grid authority, so it parses every byte and
    /// hands back a semantic checkpoint instead of the raw tail.
    fn attach_checkpoint(
        bytes: &[u8],
        geometry: Geometry,
    ) -> crate::usecase::application::terminal_session::TerminalAttachScreen {
        use usagi_core::usecase::vt_screen::VtScreen;

        let mut screen = VtScreen::new(usize::from(geometry.rows), usize::from(geometry.cols));
        screen.advance(bytes);
        crate::usecase::application::terminal_session::TerminalAttachScreen::Checkpoint(Box::new(
            screen.checkpoint(),
        ))
    }

    /// A streaming agent port whose PTY attaches live from `replay`, then reports
    /// the configured safe error on poll. It records each detach so the auto-close
    /// path can be asserted end to end.
    struct ScriptedAgentPort {
        terminal: TerminalRef,
        subscription: u64,
        replay: Vec<u8>,
        poll_error: Option<TerminalError>,
        detaches: Arc<Mutex<Vec<u64>>>,
    }

    #[coverage(off)] // coverage: reason=generic_monomorphization owner=tui expires=2027-01-31 tests=terminal_reconnect_fake_port_contract
    impl AgentCommandPort for ScriptedAgentPort {
        fn launch(
            &mut self,
            _operation: OperationId,
            _workspace: WorkspaceId,
            _session: Option<SessionId>,
            _profile: Option<AgentProfileId>,
        ) -> Result<AgentPaneAdmission, String> {
            Ok(AgentPaneAdmission {
                terminal: self.terminal.clone(),
                continuation: None,
            })
        }

        fn attach_terminal(
            &mut self,
            _terminal: &TerminalRef,
            geometry: Geometry,
        ) -> Result<TerminalAttach, TerminalError> {
            Ok(TerminalAttach {
                subscription: TerminalSubscription {
                    id: self.subscription,
                    epoch: 1,
                },
                revision: 1,
                output_offset: self.replay.len() as u64,
                next_input_seq: None,
                screen: attach_checkpoint(&self.replay, geometry),
                exited: false,
            })
        }

        fn poll_terminal(
            &mut self,
            _terminal: &TerminalRef,
            _after_offset: u64,
        ) -> Result<Vec<TerminalChunk>, TerminalError> {
            self.poll_error.map_or(Ok(Vec::new()), Err)
        }

        fn input_terminal(
            &mut self,
            _terminal: &TerminalRef,
            _subscription: TerminalSubscription,
            _input_seq: u64,
            _operation: OperationId,
            bytes: &[u8],
        ) -> Result<TerminalInputOutcome, TerminalError> {
            if bytes == b"fail" {
                Err(TerminalError::Unavailable)
            } else {
                Ok(TerminalInputOutcome::Written)
            }
        }

        fn detach_terminal(&mut self, _terminal: &TerminalRef, subscription: TerminalSubscription) {
            self.detaches.lock().unwrap().push(subscription.id);
        }
    }

    struct WheelRecordingPort {
        terminal: TerminalRef,
        replay: Vec<u8>,
        inputs: Arc<Mutex<Vec<Vec<u8>>>>,
        input_error: bool,
    }

    impl AgentCommandPort for WheelRecordingPort {
        fn launch(
            &mut self,
            _operation: OperationId,
            _workspace: WorkspaceId,
            _session: Option<SessionId>,
            _profile: Option<AgentProfileId>,
        ) -> Result<AgentPaneAdmission, String> {
            Ok(AgentPaneAdmission {
                terminal: self.terminal.clone(),
                continuation: None,
            })
        }

        fn attach_terminal(
            &mut self,
            _terminal: &TerminalRef,
            geometry: Geometry,
        ) -> Result<TerminalAttach, TerminalError> {
            Ok(TerminalAttach {
                subscription: TerminalSubscription { id: 1, epoch: 1 },
                revision: 1,
                output_offset: self.replay.len() as u64,
                next_input_seq: None,
                screen: attach_checkpoint(&self.replay, geometry),
                exited: false,
            })
        }

        fn poll_terminal(
            &mut self,
            _terminal: &TerminalRef,
            _after_offset: u64,
        ) -> Result<Vec<TerminalChunk>, TerminalError> {
            Ok(Vec::new())
        }

        fn input_terminal(
            &mut self,
            _terminal: &TerminalRef,
            _subscription: TerminalSubscription,
            _input_seq: u64,
            _operation: OperationId,
            bytes: &[u8],
        ) -> Result<TerminalInputOutcome, TerminalError> {
            if self.input_error {
                return Err(TerminalError::Unavailable);
            }
            self.inputs.lock().unwrap().push(bytes.to_vec());
            Ok(TerminalInputOutcome::Written)
        }
    }

    fn live_terminal_ref(workspace: WorkspaceId, session: SessionId) -> TerminalRef {
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: workspace,
            session_id: Some(session),
            worktree_id: WorktreeId::new(),
        }
    }

    /// Build a `WorkspaceUi` + `WorkspaceRuntime` with `port` as the daemon
    /// transport, driven into Closeup with a focused live tab attached to
    /// `terminal`. Mirrors the shell's launch → complete → focus → attach path.
    fn focused_live_pane(
        workspace: WorkspaceId,
        session: SessionId,
        terminal: TerminalRef,
        port: Box<dyn AgentCommandPort>,
    ) -> (WorkspaceUi, WorkspaceRuntime) {
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(workspace, vec![session], port);
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        // The first managed session is already selected; Enter activates it.
        let _ = runtime.handle_key(Key::Enter);
        let operation = OperationId::new();
        let _ = runtime.request_pane(Target::Session(session), operation, PaneKind::Agent);
        let _ = runtime.complete_pane(Target::Session(session), operation, terminal.clone());
        let _ = runtime.focus_terminal(Target::Session(session), terminal.clone());
        ui.start_terminal_session(terminal, terminal_geometry(20, 80));
        (ui, runtime)
    }

    #[test]
    fn an_exited_terminal_auto_closes_its_pane_and_detaches_through_the_runtime() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let terminal = live_terminal_ref(workspace, session);
        let detaches = Arc::new(Mutex::new(Vec::new()));
        let (mut ui, mut runtime) = focused_live_pane(
            workspace,
            session,
            terminal.clone(),
            Box::new(ScriptedAgentPort {
                terminal: terminal.clone(),
                subscription: 5,
                replay: b"live!".to_vec(),
                poll_error: Some(TerminalError::Exited),
                detaches: Arc::clone(&detaches),
            }),
        );
        assert!(runtime.state().has_live_pane());

        // The per-frame poll sweep observes the exit, drops the tab, and detaches
        // the client subscription — the #1011 behavior lost in the migration.
        close_exited_panes(&mut ui, &mut runtime);

        assert!(runtime.active_pane().tabs().is_empty());
        assert!(!runtime.state().has_live_pane());
        assert_eq!(*detaches.lock().unwrap(), vec![5]);
        assert!(
            ui.take_agent_exit_observation_request(),
            "an Agent exit must refresh sidebar and Garden membership immediately"
        );
    }

    /// What the shell asked of the daemon for each pane, so a test can assert
    /// that a detached background tab costs no attach and no resume.
    #[derive(Default)]
    struct BackgroundLaneLog {
        attaches: Vec<TerminalRef>,
        polls: Vec<TerminalRef>,
        watched: Vec<Vec<TerminalRef>>,
    }

    /// A port whose background lane is scripted: `exited` is what the bounded
    /// per-scope inventory has observed, drained the way the production pump
    /// hands its queue to the render thread.
    struct BackgroundLanePort {
        log: Arc<Mutex<BackgroundLaneLog>>,
        exited: Arc<Mutex<Vec<TerminalRef>>>,
    }

    impl AgentCommandPort for BackgroundLanePort {
        fn launch(
            &mut self,
            _operation: OperationId,
            _workspace: WorkspaceId,
            _session: Option<SessionId>,
            _profile: Option<AgentProfileId>,
        ) -> Result<AgentPaneAdmission, String> {
            Err("unused".to_owned())
        }

        fn attach_terminal(
            &mut self,
            terminal: &TerminalRef,
            geometry: Geometry,
        ) -> Result<TerminalAttach, TerminalError> {
            self.log.lock().unwrap().attaches.push(terminal.clone());
            Ok(TerminalAttach {
                subscription: TerminalSubscription { id: 1, epoch: 1 },
                revision: 1,
                output_offset: 0,
                next_input_seq: None,
                screen: attach_checkpoint(b"", geometry),
                exited: false,
            })
        }

        fn poll_terminal(
            &mut self,
            terminal: &TerminalRef,
            _after_offset: u64,
        ) -> Result<Vec<TerminalChunk>, TerminalError> {
            self.log.lock().unwrap().polls.push(terminal.clone());
            Ok(Vec::new())
        }

        fn watch_background_terminals(&mut self, terminals: &[TerminalRef]) {
            self.log.lock().unwrap().watched.push(terminals.to_vec());
        }

        fn take_exited_background_terminals(&mut self, limit: usize) -> Vec<TerminalRef> {
            let mut exited = self.exited.lock().unwrap();
            let taken = exited.len().min(limit);
            exited.drain(..taken).collect()
        }
    }

    /// A focused foreground tab plus one background tab in the same target, the
    /// shape #506 leaves behind: only the selection is attached.
    fn foreground_and_background_panes(
        port: Box<dyn AgentCommandPort>,
    ) -> (WorkspaceUi, WorkspaceRuntime, TerminalRef, TerminalRef) {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let background = live_terminal_ref(workspace, session);
        let foreground = live_terminal_ref(workspace, session);
        let (mut ui, mut runtime) = focused_live_pane(workspace, session, background.clone(), port);
        let operation = OperationId::new();
        let _ = runtime.request_pane(Target::Session(session), operation, PaneKind::Agent);
        let _ = runtime.complete_pane(Target::Session(session), operation, foreground.clone());
        let _ = runtime.focus_terminal(Target::Session(session), foreground.clone());
        // The shell keeps exactly the selection attached; the first tab is now a
        // detached background tab.
        ui.sync_foreground_terminal(Some(&foreground), terminal_geometry(20, 80));
        (ui, runtime, foreground, background)
    }

    #[test]
    fn a_background_tab_is_watched_by_scope_inventory_and_never_attached_or_resumed() {
        let log = Arc::new(Mutex::new(BackgroundLaneLog::default()));
        let exited = Arc::new(Mutex::new(Vec::new()));
        let (mut ui, mut runtime, foreground, background) =
            foreground_and_background_panes(Box::new(BackgroundLanePort {
                log: Arc::clone(&log),
                exited: Arc::clone(&exited),
            }));

        close_exited_panes(&mut ui, &mut runtime);

        let recorded = log.lock().unwrap();
        assert_eq!(
            recorded.watched.last().cloned(),
            Some(vec![background.clone()]),
            "only the detached background tab is observed by scope inventory"
        );
        assert!(
            !recorded.polls.iter().any(|polled| polled == &background),
            "a background tab is never resumed"
        );
        assert!(
            !recorded
                .attaches
                .iter()
                .skip(1)
                .any(|attached| attached == &background),
            "a background tab is never re-attached once it leaves the foreground"
        );
        assert_eq!(
            recorded.polls,
            vec![foreground.clone()],
            "only the foreground selection is resumed"
        );
        assert_eq!(
            runtime.active_pane().tabs().len(),
            2,
            "neither tab is closed while both runtimes are live"
        );
    }

    #[test]
    fn a_background_exit_observed_by_scope_inventory_closes_that_tab_only() {
        let log = Arc::new(Mutex::new(BackgroundLaneLog::default()));
        let exited = Arc::new(Mutex::new(Vec::new()));
        let (mut ui, mut runtime, foreground, background) =
            foreground_and_background_panes(Box::new(BackgroundLanePort {
                log: Arc::clone(&log),
                exited: Arc::clone(&exited),
            }));
        // The bounded inventory lane observed the background shell exiting.
        exited.lock().unwrap().push(background.clone());

        close_exited_panes(&mut ui, &mut runtime);

        let tabs = runtime.active_pane().tabs().to_vec();
        assert_eq!(tabs.len(), 1, "only the exited background tab is closed");
        assert!(
            matches!(&tabs[0], PaneTab::Live(live) if live.terminal.fences(&foreground)),
            "the foreground selection keeps streaming"
        );
        assert!(runtime.state().has_live_pane());
        assert!(
            ui.take_agent_exit_observation_request(),
            "a background Agent exit must wake the coherent inventory lane"
        );
        // The closed tab stops being watched on the next frame.
        close_exited_panes(&mut ui, &mut runtime);
        assert_eq!(
            log.lock().unwrap().watched.last().cloned(),
            Some(Vec::new())
        );
    }

    #[test]
    fn background_exits_are_applied_at_a_bounded_rate_per_frame() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let log = Arc::new(Mutex::new(BackgroundLaneLog::default()));
        let exited = Arc::new(Mutex::new(Vec::new()));
        let first = live_terminal_ref(workspace, session);
        let (mut ui, mut runtime) = focused_live_pane(
            workspace,
            session,
            first.clone(),
            Box::new(BackgroundLanePort {
                log: Arc::clone(&log),
                exited: Arc::clone(&exited),
            }),
        );
        let mut background = vec![first];
        for _ in 0..MAX_BACKGROUND_EXITS_PER_FRAME + 3 {
            let terminal = live_terminal_ref(workspace, session);
            let operation = OperationId::new();
            let _ = runtime.request_pane(Target::Session(session), operation, PaneKind::Agent);
            let _ = runtime.complete_pane(Target::Session(session), operation, terminal.clone());
            background.push(terminal);
        }
        let foreground = background.pop().expect("the last tab stays selected");
        let _ = runtime.focus_terminal(Target::Session(session), foreground.clone());
        ui.sync_foreground_terminal(Some(&foreground), terminal_geometry(20, 80));
        exited.lock().unwrap().extend(background.iter().cloned());

        close_exited_panes(&mut ui, &mut runtime);
        assert_eq!(
            runtime.active_pane().tabs().len(),
            background.len() + 1 - MAX_BACKGROUND_EXITS_PER_FRAME,
            "one frame applies at most the bounded slice of background exits"
        );
        // The remainder lands on the following frames, none of it lost.
        close_exited_panes(&mut ui, &mut runtime);
        close_exited_panes(&mut ui, &mut runtime);
        assert_eq!(runtime.active_pane().tabs().len(), 1);
        assert!(runtime.state().has_live_pane());
    }

    /// The wire traffic one shared connection recorded, as `e<epoch> <op> <label>`.
    type SharedConnectionLog = Arc<Mutex<Vec<String>>>;
    /// The bytes each labelled pane wrote to its PTY, in order.
    type SharedConnectionWrites = Arc<Mutex<Vec<(&'static str, Vec<u8>)>>>;

    /// Failures armed between steps of the shared-connection scenario, so each
    /// one happens at exactly the point the test drives it.
    #[derive(Default)]
    struct SharedConnectionScript {
        /// Terminals whose next poll answers `resync_required`.
        poll_resync: Vec<&'static str>,
        /// Terminals whose next viewport resize fails on the resize lane.
        resize_failures: Vec<&'static str>,
        /// Terminals whose next input loses the transport mid-response.
        input_transport_eof: Vec<&'static str>,
    }

    /// One shared daemon connection carrying every pane's attach / input /
    /// detach, as the production adapter does.
    ///
    /// Replacing that connection releases **all** of its attachments and starts
    /// a fresh per-connection input ledger — the daemon's own behavior — so a
    /// subscription taken before the replacement is no longer usable by anyone.
    /// Every request is recorded as `e<epoch> <op> <label>` so each pane's
    /// ordering within an epoch can be asserted.
    struct SharedConnectionPort {
        labels: Vec<(TerminalRef, &'static str)>,
        epoch: u64,
        next_subscription: u64,
        /// The subscriptions the live connection holds, as the daemon sees them.
        attached: Vec<(TerminalRef, u64)>,
        /// The next input sequence the daemon expects on this connection.
        ledger: Vec<(TerminalRef, u64)>,
        /// Durable input operations the daemon recorded. Unlike `ledger` this
        /// survives the connection, which is what lets a client resolve an
        /// acknowledgement it lost (#519).
        recorded_operations: Vec<OperationId>,
        script: Arc<Mutex<SharedConnectionScript>>,
        log: SharedConnectionLog,
        writes: SharedConnectionWrites,
    }

    impl SharedConnectionPort {
        fn label(&self, terminal: &TerminalRef) -> &'static str {
            self.labels
                .iter()
                .find(|(candidate, _)| candidate.fences(terminal))
                .map(|(_, label)| *label)
                .expect("every terminal in this scenario is labelled")
        }

        fn record(&self, event: String) {
            self.log.lock().unwrap().push(event);
        }

        /// Consumes one armed failure for `label`.
        fn take_armed(
            &self,
            label: &'static str,
            select: fn(&mut SharedConnectionScript) -> &mut Vec<&'static str>,
        ) -> bool {
            let mut script = self.script.lock().unwrap();
            let list = select(&mut script);
            match list.iter().position(|entry| *entry == label) {
                Some(index) => {
                    list.remove(index);
                    true
                }
                None => false,
            }
        }

        /// The transport broke mid-request: the daemon drops every attachment of
        /// that connection, and the client's next request runs on a new one.
        fn replace_transport(&mut self) {
            self.epoch += 1;
            self.attached.clear();
            self.ledger.clear();
            self.record(format!("e{} replaced", self.epoch));
        }

        fn holds(&self, terminal: &TerminalRef, subscription: u64) -> bool {
            self.attached
                .iter()
                .any(|(attached, id)| attached.fences(terminal) && *id == subscription)
        }

        fn expected_seq(&self, terminal: &TerminalRef) -> u64 {
            self.ledger
                .iter()
                .find(|(attached, _)| attached.fences(terminal))
                .map_or(0, |(_, seq)| *seq)
        }
    }

    impl AgentCommandPort for SharedConnectionPort {
        fn launch(
            &mut self,
            _operation: OperationId,
            _workspace: WorkspaceId,
            _session: Option<SessionId>,
            _profile: Option<AgentProfileId>,
        ) -> Result<AgentPaneAdmission, String> {
            Err("this scenario attaches already-launched terminals".to_owned())
        }

        fn terminal_connection_epoch(&self) -> Option<u64> {
            Some(self.epoch)
        }

        fn resize_terminal(
            &mut self,
            terminal: &TerminalRef,
            geometry: Geometry,
        ) -> Result<Geometry, TerminalError> {
            let label = self.label(terminal);
            // `Resize` rides its own deadline-bounded lane, so even its transport
            // failure leaves the shared connection — and every attachment on it —
            // alone.
            if self.take_armed(label, |script| &mut script.resize_failures) {
                self.record(format!("e{} resize-failed {label}", self.epoch));
                return Err(TerminalError::Unavailable);
            }
            self.record(format!("e{} resize {label}", self.epoch));
            Ok(geometry)
        }

        fn attach_terminal(
            &mut self,
            terminal: &TerminalRef,
            geometry: Geometry,
        ) -> Result<TerminalAttach, TerminalError> {
            let label = self.label(terminal);
            self.next_subscription += 1;
            let id = self.next_subscription;
            self.attached.push((terminal.clone(), id));
            self.record(format!("e{} attach {label}", self.epoch));
            Ok(TerminalAttach {
                subscription: TerminalSubscription {
                    id,
                    epoch: self.epoch,
                },
                revision: 1,
                output_offset: 0,
                next_input_seq: None,
                screen: attach_checkpoint(b"", geometry),
                exited: false,
            })
        }

        fn poll_terminal(
            &mut self,
            terminal: &TerminalRef,
            _after_offset: u64,
        ) -> Result<Vec<TerminalChunk>, TerminalError> {
            let label = self.label(terminal);
            // A fully received `resync_required` is a finished answer: it tells
            // one pane to replace its screen, not the whole TUI to reconnect.
            if self.take_armed(label, |script| &mut script.poll_resync) {
                self.record(format!("e{} resync-required {label}", self.epoch));
                return Err(TerminalError::ResyncRequired);
            }
            self.record(format!("e{} resume {label}", self.epoch));
            Ok(Vec::new())
        }

        fn input_terminal(
            &mut self,
            terminal: &TerminalRef,
            subscription: TerminalSubscription,
            input_seq: u64,
            operation: OperationId,
            bytes: &[u8],
        ) -> Result<TerminalInputOutcome, TerminalError> {
            let label = self.label(terminal);
            // What the daemon does with a subscription whose connection is gone:
            // it released that attachment, so the write is refused with no effect
            // and the keystroke is lost. No pane may ever reach this.
            if subscription.epoch != self.epoch || !self.holds(terminal, subscription.id) {
                self.record(format!("e{} not-attached {label}", self.epoch));
                return Err(TerminalError::Stale);
            }
            let expected = self.expected_seq(terminal);
            if input_seq != expected {
                self.record(format!(
                    "e{} sequence-gap {label} (got {input_seq}, want {expected})",
                    self.epoch
                ));
                return Err(TerminalError::Stale);
            }
            if self.take_armed(label, |script| &mut script.input_transport_eof) {
                // The daemon applied the write and recorded its operation; only
                // the response was lost. That is the case #519 has to converge:
                // the client must resolve the operation, not resend the bytes.
                self.apply(terminal, label, input_seq, bytes);
                self.recorded_operations.push(operation);
                self.replace_transport();
                return Err(TerminalError::InputEffectUnknown);
            }
            self.apply(terminal, label, input_seq, bytes);
            self.recorded_operations.push(operation);
            Ok(TerminalInputOutcome::Written)
        }

        fn terminal_input_outcome(
            &mut self,
            terminal: &TerminalRef,
            operation: OperationId,
            _input_len: usize,
        ) -> Result<TerminalInputResolution, TerminalError> {
            let label = self.label(terminal);
            self.record(format!("e{} input-outcome {label}", self.epoch));
            Ok(if self.recorded_operations.contains(&operation) {
                TerminalInputResolution::Final(TerminalInputOutcome::Written)
            } else {
                TerminalInputResolution::Unknown
            })
        }

        fn detach_terminal(&mut self, terminal: &TerminalRef, subscription: TerminalSubscription) {
            let label = self.label(terminal);
            if subscription.epoch != self.epoch {
                // Released locally: the daemon already dropped this attachment
                // with its connection, so nothing on the current one is touched.
                self.record(format!("e{} local-detach {label}", self.epoch));
                return;
            }
            self.attached
                .retain(|(attached, id)| !(attached.fences(terminal) && *id == subscription.id));
            self.record(format!("e{} detach {label}", self.epoch));
        }
    }

    impl SharedConnectionPort {
        /// Records one accepted write exactly as the daemon would.
        fn apply(
            &mut self,
            terminal: &TerminalRef,
            label: &'static str,
            input_seq: u64,
            bytes: &[u8],
        ) {
            match self
                .ledger
                .iter_mut()
                .find(|(attached, _)| attached.fences(terminal))
            {
                Some((_, seq)) => *seq += 1,
                None => self.ledger.push((terminal.clone(), 1)),
            }
            self.writes.lock().unwrap().push((label, bytes.to_vec()));
            self.record(format!("e{} input#{input_seq} {label}", self.epoch));
        }
    }

    /// Every attachment-fenced request one pane made in one epoch, in order.
    fn fenced_traffic(log: &[String], epoch: &str, label: &str) -> Vec<String> {
        log.iter()
            .filter(|event| {
                event.starts_with(epoch)
                    && event.ends_with(label)
                    && (event.contains(" attach ")
                        || event.contains(" resume ")
                        || event.contains(" input#"))
            })
            .cloned()
            .collect()
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One scenario drives every epoch transition in order.
    fn a_replaced_shared_connection_reattaches_every_pane_before_it_streams_again() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let agent = live_terminal_ref(workspace, session);
        let generic = live_terminal_ref(workspace, session);
        let script = Arc::new(Mutex::new(SharedConnectionScript::default()));
        let log = Arc::new(Mutex::new(Vec::new()));
        let writes = Arc::new(Mutex::new(Vec::new()));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(
                workspace,
                vec![session],
                Box::new(SharedConnectionPort {
                    labels: vec![(agent.clone(), "A"), (generic.clone(), "B")],
                    epoch: 1,
                    next_subscription: 10,
                    attached: Vec::new(),
                    ledger: Vec::new(),
                    recorded_operations: Vec::new(),
                    script: Arc::clone(&script),
                    log: Arc::clone(&log),
                    writes: Arc::clone(&writes),
                }),
            );
        let geometry = terminal_geometry(20, 80);

        // Both panes attach over one connection and type once.
        ui.start_terminal_session(agent.clone(), geometry);
        ui.start_terminal_session(generic.clone(), geometry);
        assert_eq!(ui.send_terminal_bytes(&agent, b"a"), Ok(()));
        assert_eq!(ui.send_terminal_bytes(&generic, b"b"), Ok(()));

        // 1. Pane A's poll takes a fully received `resync_required`. It resyncs on
        //    the same connection, so B keeps its attachment and its ledger
        //    position, and A continues from the sequence the daemon expects.
        script.lock().unwrap().poll_resync.push("A");
        assert!(ui.poll_all_terminals().is_empty());
        assert_eq!(ui.send_terminal_bytes(&generic, b"b2"), Ok(()));
        assert_eq!(ui.send_terminal_bytes(&agent, b"a2"), Ok(()));

        // 2. A's viewport resize fails on the resize lane. Neither pane loses its
        //    attachment, so both keep writing on the same subscriptions.
        script.lock().unwrap().resize_failures.push("A");
        ui.resize_terminals(terminal_geometry(24, 100));
        assert_eq!(ui.send_terminal_bytes(&agent, b"a3"), Ok(()));

        // 3. A's input loses the transport before its response completes. The
        //    daemon released B's attachment with that connection too, even though
        //    B never saw a failure.
        script.lock().unwrap().input_transport_eof.push("A");
        assert!(ui.send_terminal_bytes(&agent, b"a4").is_err());

        // B's very next keystroke attaches on the new connection first, and is
        // written exactly once instead of being rejected as unattached.
        assert_eq!(ui.send_terminal_bytes(&generic, b"k"), Ok(()));

        // A recovers through its own reconnect backoff, then both panes stream.
        for _ in 0..200 {
            if ui.poll_all_terminals().is_empty()
                && fenced_traffic(&log.lock().unwrap(), "e2", "A")
                    .iter()
                    .any(|event| event.contains(" attach "))
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // A's lost acknowledgement fenced its producer queue, so reattaching is
        // not enough: the next tick resolves that operation against the daemon's
        // durable record before any later keystroke may reach the PTY (#519).
        assert_eq!(
            ui.send_terminal_bytes(&agent, b"a4-next"),
            Err(
                "terminal input is held in order behind an unresolved input (1 waiting)".to_owned()
            )
        );
        ui.poll_all_terminals();
        assert!(
            log.lock()
                .unwrap()
                .iter()
                .any(|event| event == "e2 input-outcome A")
        );
        // The held keystroke was delivered in order once the fence converged.
        assert_eq!(
            writes.lock().unwrap().last(),
            Some(&("A", b"a4-next".to_vec()))
        );
        assert_eq!(ui.send_terminal_bytes(&agent, b"a5"), Ok(()));

        // Releasing A's pane at the end must not disturb B's attachment.
        ui.close_terminal(&agent);
        assert_eq!(ui.send_terminal_bytes(&generic, b"k2"), Ok(()));
        // Returning to A on the same connection revives its retained coordinator
        // and continues at the daemon ledger cursor instead of restarting at 0.
        ui.start_terminal_session(agent.clone(), geometry);
        assert_eq!(ui.send_terminal_bytes(&agent, b"a6"), Ok(()));

        let log = log.lock().unwrap().clone();
        // No keystroke was ever spent on a released subscription, and no ledger
        // gap opened: the exact cascade this fences off.
        assert!(
            !log.iter()
                .any(|event| event.contains("not-attached") || event.contains("sequence-gap")),
            "{log:#?}"
        );
        // In each epoch, every pane's first attachment-fenced request is its own
        // attach — never a `Resume` or an `Input` on a released subscription.
        for epoch in ["e1", "e2"] {
            for label in ["A", "B"] {
                let traffic = fenced_traffic(&log, epoch, label);
                assert_eq!(
                    traffic.first(),
                    Some(&format!("{epoch} attach {label}")),
                    "{label} in {epoch}: {log:#?}"
                );
            }
        }
        // Exactly one connection replacement happened, and only the failing lane
        // caused it.
        assert_eq!(
            log.iter()
                .filter(|event| event.contains("replaced"))
                .count(),
            1,
            "{log:#?}"
        );
        // B held a subscription from the replaced connection, so its release was
        // local: it was never re-sent on the connection its peers now use, and it
        // came after — and did not revoke — the attach that replaced it.
        assert!(log.contains(&"e2 local-detach B".to_owned()), "{log:#?}");
        // A's same-connection resync detached its own superseded subscription
        // there, where the daemon still held it, and closing A's pane later
        // released its current one the same way.
        assert!(log.contains(&"e1 detach A".to_owned()), "{log:#?}");
        assert!(log.contains(&"e2 detach A".to_owned()), "{log:#?}");
        // Sequences continue across a same-connection resync and restart only on
        // the new connection's fresh ledger.
        for (label, expected) in [
            (
                'A',
                vec![
                    "e1 input#0 A",
                    "e1 input#1 A",
                    "e1 input#2 A",
                    // The write whose acknowledgement was lost: the daemon
                    // applied it once, which is exactly what the client cannot
                    // know until it resolves the operation.
                    "e1 input#3 A",
                    // The held keystroke, then the one typed after the fence
                    // converged. Both on the fresh epoch's restarted sequence.
                    "e2 input#0 A",
                    "e2 input#1 A",
                    // Detach/re-attach on e2 retains the coordinator ledger.
                    "e2 input#2 A",
                ],
            ),
            (
                'B',
                vec![
                    "e1 input#0 B",
                    "e1 input#1 B",
                    "e2 input#0 B",
                    "e2 input#1 B",
                ],
            ),
        ] {
            assert_eq!(
                log.iter()
                    .filter(|event| event.contains(" input#") && event.ends_with(label))
                    .cloned()
                    .collect::<Vec<_>>(),
                expected,
                "{log:#?}"
            );
        }

        // Every keystroke reached the PTY once, in order, including the first one
        // after the recovery.
        assert_eq!(
            writes.lock().unwrap().clone(),
            vec![
                ("A", b"a".to_vec()),
                ("B", b"b".to_vec()),
                ("B", b"b2".to_vec()),
                ("A", b"a2".to_vec()),
                ("A", b"a3".to_vec()),
                // Applied before the response was lost, and never applied twice.
                ("A", b"a4".to_vec()),
                ("B", b"k".to_vec()),
                // Released from the fence in production order.
                ("A", b"a4-next".to_vec()),
                ("A", b"a5".to_vec()),
                ("B", b"k2".to_vec()),
                ("A", b"a6".to_vec()),
            ]
        );
    }

    #[test]
    fn reconnecting_and_stale_terminal_states_are_projected_into_the_pane_footer() {
        for (error, expected) in [
            (
                TerminalError::Unavailable,
                "daemon unavailable; reconnecting",
            ),
            (TerminalError::Stale, "terminal is no longer available"),
        ] {
            let workspace = WorkspaceId::new();
            let session = SessionId::new();
            let terminal = live_terminal_ref(workspace, session);
            let (mut ui, runtime) = focused_live_pane(
                workspace,
                session,
                terminal.clone(),
                Box::new(ScriptedAgentPort {
                    terminal,
                    subscription: 6,
                    replay: b"retained".to_vec(),
                    poll_error: Some(error),
                    detaches: Arc::new(Mutex::new(Vec::new())),
                }),
            );
            let mut controls = LiveTerminalControls::default();

            assert!(ui.poll_all_terminals().is_empty());
            let view = controller_terminal_view(&ui, &runtime, &mut controls, 10).unwrap();

            assert_eq!(view.feedback.as_deref(), Some(expected));
            assert_eq!(view.rows[0], "retained");
        }
    }

    #[test]
    fn terminal_reconnect_fake_port_contract() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let terminal = live_terminal_ref(workspace, session);
        let (mut ui, _runtime) = focused_live_pane(
            workspace,
            session,
            terminal.clone(),
            Box::new(ScriptedAgentPort {
                terminal,
                subscription: 17,
                replay: Vec::new(),
                poll_error: Some(TerminalError::Unavailable),
                detaches: Arc::new(Mutex::new(Vec::new())),
            }),
        );

        assert!(ui.poll_all_terminals().is_empty());
        assert!(!ui.take_terminal_reconnected());
        std::thread::sleep(std::time::Duration::from_millis(110));
        assert!(ui.poll_all_terminals().is_empty());
        assert!(ui.take_terminal_reconnected());
        assert!(!ui.take_terminal_reconnected());
    }

    #[test]
    fn close_tab_live_action_keeps_the_focused_agent_attached() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let terminal = live_terminal_ref(workspace, session);
        let detaches = Arc::new(Mutex::new(Vec::new()));
        let (ui, mut runtime) = focused_live_pane(
            workspace,
            session,
            terminal.clone(),
            Box::new(ScriptedAgentPort {
                terminal,
                subscription: 8,
                replay: Vec::new(),
                poll_error: None,
                detaches: Arc::clone(&detaches),
            }),
        );
        let mut controls = LiveTerminalControls::default();
        let mut term = FakeTerminal::default();
        let mut browser = UnavailableBrowserOpener;
        let mut ui = ui;
        let mut pending_targets = std::collections::HashMap::new();

        assert!(intercept_live_terminal_control(
            &Key::Live(LiveTerminalAction::CloseTab),
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut pending_targets,
            20,
            80,
            0,
            0,
        ));

        assert_eq!(runtime.active_pane().tabs().len(), 1);
        assert!(detaches.lock().unwrap().is_empty());
        assert_eq!(
            runtime
                .state()
                .notice()
                .map(|notice| notice.message.as_str()),
            Some("Agent tabs stay visible; exit the Agent with Ctrl-D")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One fixture covers every wheel route with shared pane geometry.
    fn physical_wheel_follows_full_screen_program_input_modes() {
        let cases = [
            (
                b"\x1b[?1000h\x1b[?1006hclaude".as_slice(),
                Some(b"\x1b[<64;5;1M".to_vec()),
            ),
            (
                b"\x1b[?1049h\x1b[?1hcodex".as_slice(),
                Some(b"\x1bOA".repeat(super::WHEEL_LINES)),
            ),
            (b"\x1b[?1000hclaude".as_slice(), None),
        ];

        for (replay, expected) in cases {
            let workspace = WorkspaceId::new();
            let session = SessionId::new();
            let terminal = live_terminal_ref(workspace, session);
            let inputs = Arc::new(Mutex::new(Vec::new()));
            let (mut ui, mut runtime) = focused_live_pane(
                workspace,
                session,
                terminal.clone(),
                Box::new(WheelRecordingPort {
                    terminal,
                    replay: replay.to_vec(),
                    inputs: Arc::clone(&inputs),
                    input_error: expected.is_none(),
                }),
            );
            let mut controls = LiveTerminalControls::default();
            let geometry = terminal_geometry(20, 80);
            let (_, rows_len, scroll) =
                poll_and_project_terminals(&mut ui, &mut runtime, &mut controls, geometry);
            let mut term = FakeTerminal::default();
            let mut browser = RecordingBrowser::default();
            let mut pending = std::collections::HashMap::new();

            assert!(intercept_live_terminal_control(
                &Key::Live(LiveTerminalAction::Wheel {
                    up: true,
                    column: 41,
                    row: 5,
                }),
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &mut browser,
                &mut pending,
                20,
                80,
                rows_len,
                scroll,
            ));
            assert_eq!(inputs.lock().unwrap().as_slice(), expected.as_slice());
            if expected.is_none() {
                assert!(controls.project(Vec::new(), 1).feedback.is_some());
            }
        }

        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let terminal = live_terminal_ref(workspace, session);
        let inputs = Arc::new(Mutex::new(Vec::new()));
        let mut replay = String::new();
        for row in 0..30 {
            use std::fmt::Write as _;
            let _ = writeln!(replay, "row {row}\r");
        }
        let replay = replay.into_bytes();
        let (mut ui, mut runtime) = focused_live_pane(
            workspace,
            session,
            terminal.clone(),
            Box::new(WheelRecordingPort {
                terminal: terminal.clone(),
                replay,
                inputs: Arc::clone(&inputs),
                input_error: false,
            }),
        );
        let mut controls = LiveTerminalControls::default();
        let geometry = terminal_geometry(20, 80);
        let (_, rows_len, scroll) =
            poll_and_project_terminals(&mut ui, &mut runtime, &mut controls, geometry);
        let mut term = FakeTerminal::default();
        let mut browser = RecordingBrowser::default();
        let mut pending = std::collections::HashMap::new();
        assert!(intercept_live_terminal_control(
            &Key::Live(LiveTerminalAction::Wheel {
                up: true,
                column: 0,
                row: 0,
            }),
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut pending,
            20,
            80,
            rows_len,
            scroll,
        ));
        assert!(intercept_live_terminal_control(
            &Key::Live(LiveTerminalAction::Wheel {
                up: true,
                column: 41,
                row: 5,
            }),
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut pending,
            20,
            80,
            rows_len,
            scroll,
        ));
        let (view, _, _) =
            poll_and_project_terminals(&mut ui, &mut runtime, &mut controls, geometry);
        assert_eq!(view.expect("primary history").scroll, super::WHEEL_LINES);
        assert!(inputs.lock().unwrap().is_empty());

        assert!(intercept_live_terminal_control(
            &Key::Live(LiveTerminalAction::Wheel {
                up: false,
                column: 41,
                row: 5,
            }),
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut pending,
            20,
            80,
            rows_len,
            scroll,
        ));
        let (view, _, _) =
            poll_and_project_terminals(&mut ui, &mut runtime, &mut controls, geometry);
        assert_eq!(view.expect("primary history").scroll, 0);

        ui.close_terminal(&terminal);
        assert!(intercept_live_terminal_control(
            &Key::Live(LiveTerminalAction::Wheel {
                up: true,
                column: 41,
                row: 5,
            }),
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut pending,
            20,
            80,
            rows_len,
            scroll,
        ));

        let empty_view = WorkspaceView::with_runtime_ids(ws("empty"), state("empty"), vec![]);
        let mut empty_ui = WorkspaceUi::new(empty_view, Box::new(UnavailableSessionCommandPort));
        let mut empty_runtime = WorkspaceRuntime::new(WorkspaceId::new(), vec![]);
        let _ = empty_runtime.handle_key(Key::Live(LiveTerminalAction::Director));
        let drawer = crate::presentation::director_drawer::geometry(20, 80);
        assert!(intercept_live_terminal_control(
            &Key::Live(LiveTerminalAction::Wheel {
                up: true,
                column: u16::try_from(drawer.left.saturating_add(2)).expect("drawer column"),
                row: u16::try_from(drawer.top.saturating_add(4)).expect("drawer row"),
            }),
            &mut empty_ui,
            &mut empty_runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut pending,
            20,
            80,
            rows_len,
            scroll,
        ));
    }

    /// `Ctrl-O b` is the way back to live output. A scrolled viewport holds its
    /// rows against everything the Agent appends, so the distance to the newest
    /// output grows with the conversation and one-line `ScrollDown` alone cannot
    /// be the only way back.
    #[test]
    fn scroll_bottom_returns_a_scrolled_pane_to_the_live_output() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let terminal = live_terminal_ref(workspace, session);
        let mut replay = String::new();
        for row in 0..40 {
            use std::fmt::Write as _;
            let _ = writeln!(replay, "row {row}\r");
        }
        let replay = replay.into_bytes();
        let (mut ui, mut runtime) = focused_live_pane(
            workspace,
            session,
            terminal.clone(),
            Box::new(ScriptedAgentPort {
                terminal,
                subscription: 11,
                replay,
                poll_error: None,
                detaches: Arc::new(Mutex::new(Vec::new())),
            }),
        );
        let geometry = terminal_geometry(20, 80);
        let mut controls = LiveTerminalControls::default();
        let mut term = FakeTerminal::default();
        let mut browser = RecordingBrowser::default();
        let mut pending = std::collections::HashMap::new();
        let mut scroll_key = |key,
                              ui: &mut WorkspaceUi,
                              runtime: &mut WorkspaceRuntime,
                              controls: &mut LiveTerminalControls,
                              rows_len,
                              scroll| {
            assert!(intercept_live_terminal_control(
                &Key::Live(key),
                ui,
                runtime,
                controls,
                &mut term,
                &mut browser,
                &mut pending,
                20,
                80,
                rows_len,
                scroll,
            ));
        };

        let (view, rows_len, scroll) =
            poll_and_project_terminals(&mut ui, &mut runtime, &mut controls, geometry);
        let live_bottom = view.expect("the focused live tab projects its rows");
        assert_eq!(live_bottom.scroll, 0);

        for _ in 0..5 {
            scroll_key(
                LiveTerminalAction::ScrollUp,
                &mut ui,
                &mut runtime,
                &mut controls,
                rows_len,
                scroll,
            );
        }
        let (scrolled, rows_len, scroll) =
            poll_and_project_terminals(&mut ui, &mut runtime, &mut controls, geometry);
        let scrolled = scrolled.expect("a scrolled viewport still projects rows");
        assert_eq!(scrolled.scroll, 5);
        assert_ne!(scrolled.rows, live_bottom.rows);

        scroll_key(
            LiveTerminalAction::ScrollBottom,
            &mut ui,
            &mut runtime,
            &mut controls,
            rows_len,
            scroll,
        );
        let (followed, _, _) =
            poll_and_project_terminals(&mut ui, &mut runtime, &mut controls, geometry);
        assert_eq!(
            followed.expect("the pane follows live output again"),
            live_bottom
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One fixture audits every pane-only and reducer-owned key.
    fn switch_consumes_right_pane_controls_without_mutating_the_dimmed_pane() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let terminal = live_terminal_ref(workspace, session);
        let detaches = Arc::new(Mutex::new(Vec::new()));
        let (mut ui, mut runtime) = focused_live_pane(
            workspace,
            session,
            terminal.clone(),
            Box::new(ScriptedAgentPort {
                terminal,
                subscription: 18,
                replay: b"one\ntwo\nthree\nhttps://example.com".to_vec(),
                poll_error: None,
                detaches: Arc::clone(&detaches),
            }),
        );
        let mut controls = LiveTerminalControls::default();
        let rows = vec![
            "one".to_owned(),
            "two".to_owned(),
            "three".to_owned(),
            "https://example.com".to_owned(),
        ];
        let _ = controls.project(rows.clone(), 1);
        controls.scroll_up();
        let before = controls.project(rows.clone(), 1).scroll;
        assert_eq!(before, 1);
        let tabs_before = runtime.active_pane().tabs().to_vec();

        let mut term = FakeTerminal::default();
        let mut browser = RecordingBrowser::default();
        let mut pending = std::collections::HashMap::new();
        let _ = runtime.handle_key(Key::Live(LiveTerminalAction::OpenCloseupModal));
        assert_eq!(runtime.state().overlay(), Some(Overlay::Closeup));
        assert!(intercept_live_terminal_control(
            &Key::Live(LiveTerminalAction::ScrollUp),
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut pending,
            20,
            80,
            4,
            before,
        ));
        let _ = runtime.handle_key(Key::Escape);
        assert!(!runtime.wants_live_input());
        for key in [
            Key::Live(LiveTerminalAction::ScrollUp),
            Key::Live(LiveTerminalAction::ScrollDown),
            Key::Live(LiveTerminalAction::CloseTab),
            Key::Live(LiveTerminalAction::MoveTabNext),
            Key::Live(LiveTerminalAction::MoveTabPrevious),
            Key::Pointer(PointerEvent {
                kind: PointerKind::Drag,
                column: 41,
                row: 5,
            }),
            Key::Pointer(PointerEvent {
                kind: PointerKind::Up,
                column: 41,
                row: 5,
            }),
        ] {
            assert!(intercept_live_terminal_control(
                &key,
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &mut browser,
                &mut pending,
                20,
                80,
                4,
                before,
            ));
        }

        for key in [
            Key::Live(LiveTerminalAction::NextTab),
            Key::Passthrough(Vec::new()),
            Key::TerminalCopy {
                fallback: Vec::new(),
            },
            Key::Up,
            Key::Down,
            Key::Left,
            Key::Right,
            Key::Home,
            Key::End,
            Key::Delete,
            Key::LineStart,
            Key::LineEnd,
            Key::SelectLeft,
            Key::SelectRight,
            Key::SelectHome,
            Key::SelectEnd,
            Key::Enter,
            Key::Backspace,
            Key::Tab,
            Key::Escape,
            Key::Quit,
            Key::CtrlQ,
            Key::CtrlD,
            Key::Char('x'),
            Key::Click { column: 41, row: 5 },
            Key::Other,
        ] {
            assert!(!intercept_live_terminal_control(
                &key,
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &mut browser,
                &mut pending,
                20,
                80,
                4,
                before,
            ));
        }
        assert_eq!(controls.project(rows, 1).scroll, before);
        assert!(!controls.has_selection());
        assert_eq!(runtime.active_pane().tabs(), tabs_before.as_slice());
        assert!(detaches.lock().unwrap().is_empty());
        assert!(term.copied.is_empty());
        assert!(browser.opened.is_empty());
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One shell matrix fixes drawer mouse and pane ownership.
    fn director_drawer_consumes_shell_pane_controls_without_background_mutation() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let terminal = live_terminal_ref(workspace, session);
        let (mut ui, mut runtime) = focused_live_pane(
            workspace,
            session,
            terminal.clone(),
            Box::new(ScriptedAgentPort {
                terminal,
                subscription: 181,
                replay: b"one\ntwo\nthree".to_vec(),
                poll_error: None,
                detaches: Arc::new(Mutex::new(Vec::new())),
            }),
        );
        let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
        assert!(runtime.state().director_drawer_open());
        let tabs_before = runtime.active_pane().tabs().to_vec();
        let mut controls = LiveTerminalControls::default();
        let rows = vec!["one".to_owned(), "two".to_owned(), "three".to_owned()];
        let _ = controls.project(rows.clone(), 1);
        controls.scroll_up();
        let scroll_before = controls.project(rows.clone(), 1).scroll;
        let mut term = FakeTerminal::default();
        let mut browser = RecordingBrowser::default();
        let mut pending = std::collections::HashMap::new();
        let drawer = super::director_drawer::geometry(20, 80);
        let new_click = Key::Click {
            column: u16::try_from(drawer.left + drawer.width - 3).unwrap(),
            row: u16::try_from(drawer.top + 2).unwrap(),
        };
        assert!(super::is_director_new_click(&new_click, &runtime, 20, 80));
        let new_pointer = Key::Pointer(PointerEvent {
            kind: PointerKind::Down,
            column: u16::try_from(drawer.left + drawer.width - 3).unwrap(),
            row: u16::try_from(drawer.top + 2).unwrap(),
        });
        assert!(super::is_director_new_click(&new_pointer, &runtime, 20, 80));
        assert!(!intercept_live_terminal_control(
            &new_pointer,
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut pending,
            20,
            80,
            3,
            scroll_before,
        ));
        let managed_before = runtime
            .panes()
            .pane(Target::Session(session))
            .unwrap()
            .tabs()
            .to_vec();
        let launch_effects = runtime.apply_event(AppEvent::Key(AppKey::OpenDirectorNew));
        assert!(launch_effects.is_empty());
        assert!(matches!(
            runtime.state().director_new(),
            DirectorNew::Choosing(_)
        ));
        assert_eq!(runtime.panes().active(), Some(Target::Root(workspace)));
        assert_eq!(
            runtime
                .panes()
                .pane(Target::Session(session))
                .unwrap()
                .tabs(),
            managed_before.as_slice()
        );
        assert!(!super::is_director_new_click(
            &new_pointer,
            &runtime,
            20,
            80
        ));
        assert!(intercept_live_terminal_control(
            &new_pointer,
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut pending,
            20,
            80,
            3,
            scroll_before,
        ));
        assert_eq!(
            launch_effects
                .iter()
                .filter(|effect| matches!(effect, Effect::LaunchAgent { .. }))
                .count(),
            0
        );
        assert!(intercept_live_terminal_control(
            &Key::Pointer(PointerEvent {
                kind: PointerKind::Up,
                column: u16::try_from(drawer.left + drawer.width - 3).unwrap(),
                row: u16::try_from(drawer.top + 2).unwrap(),
            }),
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut pending,
            20,
            80,
            3,
            scroll_before,
        ));
        assert!(matches!(
            runtime.state().director_new(),
            DirectorNew::Choosing(_)
        ));
        assert_eq!(
            runtime
                .panes()
                .pane(Target::Session(session))
                .unwrap()
                .tabs(),
            managed_before.as_slice()
        );

        for key in [
            Key::Live(LiveTerminalAction::NextTab),
            Key::Live(LiveTerminalAction::ScrollUp),
            Key::Live(LiveTerminalAction::ScrollDown),
            Key::Live(LiveTerminalAction::CloseTab),
            Key::Live(LiveTerminalAction::MoveTabNext),
            Key::Live(LiveTerminalAction::MoveTabPrevious),
            Key::Pointer(PointerEvent {
                kind: PointerKind::Drag,
                column: 41,
                row: 5,
            }),
        ] {
            assert!(intercept_live_terminal_control(
                &key,
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &mut browser,
                &mut pending,
                20,
                80,
                3,
                scroll_before,
            ));
        }
        assert_eq!(controls.project(rows, 1).scroll, scroll_before);
        assert_eq!(runtime.active_pane().tabs(), tabs_before.as_slice());
        assert!(term.copied.is_empty());
        assert!(browser.opened.is_empty());
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One fixture covers every admitted and refused drawer slot.
    fn director_projection_and_tab_cycle_cover_every_agent_only_slot() {
        let workspace = WorkspaceId::new();
        let live = scoped_terminal_ref(workspace, None);
        let live_continuation = AgentContinuationRef::new();
        let interrupted = interrupted_history(workspace, None, true);
        let interrupted_continuation = interrupted.continuation;
        let mut intent = AgentTabIntent::empty(workspace);
        intent.apply(AgentTabIntentMutation::Upsert {
            session_id: None,
            continuation: live_continuation,
            terminal: live.clone(),
            select: true,
        });
        intent.apply(AgentTabIntentMutation::Upsert {
            session_id: None,
            continuation: interrupted_continuation,
            terminal: interrupted.last_terminal.clone(),
            select: false,
        });
        let durable = Arc::new(Mutex::new(intent));
        let mutations = Arc::new(Mutex::new(Vec::new()));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(workspace, Vec::new(), Box::new(UnavailableAgentCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::new(),
                Box::new(MemoryIntentPort {
                    state: Arc::clone(&durable),
                    mutations: Arc::clone(&mutations),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
        let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
        assert!(super::select_director_tab(
            &Key::Live(LiveTerminalAction::NextTab),
            &mut ui,
            &mut runtime,
        ));
        let _ = runtime.handle_key(Key::Escape);
        let fence = runtime.restore_fence();
        assert!(runtime.restore_snapshot(
            fence.0,
            fence.1,
            vec![super::PaneRestoreTarget {
                target: Target::Root(workspace),
                panes: vec![LivePane {
                    terminal: live.clone(),
                    kind: PaneKind::Agent,
                }],
                selected: Some(live.clone()),
                selected_interrupted: None,
                interrupted: vec![interrupted.clone()],
            }],
        ));

        // Closed drawers deliberately project nothing.
        assert_eq!(
            super::director_drawer_projection(&ui, &runtime, None),
            super::DirectorDrawerProjection::default()
        );
        let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
        let terminal_view = TerminalViewProjection {
            rows: vec!["retained output".to_owned()],
            row_offset: 0,
            total_rows: 1,
            scroll: 0,
            feedback: Some("reconnecting".to_owned()),
        };
        let projected = super::director_drawer_projection(&ui, &runtime, Some(&terminal_view));
        assert_eq!(projected.conversations.len(), 2);
        assert!(projected.conversations[0].selected);
        assert_eq!(projected.terminal_view, Some(terminal_view));
        assert_eq!(projected.interrupted_detail, None);

        // A live projection without control feedback still crosses the seam as
        // a projection; the adapter never converts its rows into drawer lines.
        let quiet_terminal_view = TerminalViewProjection {
            rows: vec!["quiet retained output".to_owned()],
            row_offset: 0,
            total_rows: 1,
            scroll: 0,
            feedback: None,
        };
        assert_eq!(
            super::director_drawer_projection(&ui, &runtime, Some(&quiet_terminal_view))
                .terminal_view,
            Some(quiet_terminal_view)
        );

        // Closing the selected live Agent is a no-op. The daemon-owned tab and
        // selection remain intact until the CLI exits.
        let mut pending_targets = std::collections::HashMap::new();
        super::close_focused_terminal_pane(&mut ui, &mut runtime, &mut pending_targets);
        let interrupted_projection = super::director_drawer_projection(&ui, &runtime, None);
        assert!(interrupted_projection.conversations[0].selected);
        assert_eq!(interrupted_projection.interrupted_detail, None);
        assert_eq!(interrupted_projection.terminal_view, None);
        runtime.fail_tab_resume_for(
            Target::Root(workspace),
            interrupted_continuation,
            None,
            "safe retry feedback".to_owned(),
        );
        let failed_projection = super::director_drawer_projection(&ui, &runtime, None);
        assert_eq!(failed_projection.interrupted_detail, None);
        assert_eq!(
            failed_projection.feedback.as_deref(),
            Some("safe retry feedback")
        );
        assert!(super::select_director_tab(
            &Key::Live(LiveTerminalAction::NextTab),
            &mut ui,
            &mut runtime,
        ));
        let selected_interrupted = super::director_drawer_projection(&ui, &runtime, None);
        assert!(selected_interrupted.conversations[1].selected);
        assert!(selected_interrupted.interrupted_detail.is_some());
        let pending = OperationId::new();
        let _ = runtime.request_pane(Target::Root(workspace), pending, PaneKind::Agent);
        runtime.inject_pane_event_for_test(
            Target::Root(workspace),
            crate::usecase::application::pane::PaneEvent::Select(
                crate::usecase::application::pane::PaneSelection::Tab(TabSelection::Pending(
                    pending,
                )),
            ),
        );
        let pending_projection = super::director_drawer_projection(&ui, &runtime, None);
        assert!(
            pending_projection
                .conversations
                .iter()
                .any(|conversation| conversation.label == "Agent (starting)"
                    && conversation.selected)
        );

        // Bypass runtime admission to prove the projection independently drops
        // every generic/diff shape if an impossible state reaches it.
        let generic = scoped_terminal_ref(workspace, None);
        runtime.inject_pane_event_for_test(
            Target::Root(workspace),
            crate::usecase::application::pane::PaneEvent::Restore(LivePane {
                terminal: generic,
                kind: PaneKind::Terminal,
            }),
        );
        let unobserved_agent = scoped_terminal_ref(workspace, None);
        runtime.inject_pane_event_for_test(
            Target::Root(workspace),
            crate::usecase::application::pane::PaneEvent::Restore(LivePane {
                terminal: unobserved_agent.clone(),
                kind: PaneKind::Agent,
            }),
        );
        runtime.inject_pane_event_for_test(
            Target::Root(workspace),
            crate::usecase::application::pane::PaneEvent::Select(
                crate::usecase::application::pane::PaneSelection::Tab(TabSelection::Live(
                    unobserved_agent,
                )),
            ),
        );
        let generic_pending = OperationId::new();
        runtime.inject_pane_event_for_test(
            Target::Root(workspace),
            crate::usecase::application::pane::PaneEvent::Request {
                operation: generic_pending,
                target: Target::Root(workspace),
                kind: PaneKind::Terminal,
            },
        );
        let diff = OperationId::new();
        runtime.inject_pane_event_for_test(
            Target::Root(workspace),
            crate::usecase::application::pane::PaneEvent::Request {
                operation: diff,
                target: Target::Root(workspace),
                kind: PaneKind::Diff,
            },
        );
        runtime.inject_pane_event_for_test(
            Target::Root(workspace),
            crate::usecase::application::pane::PaneEvent::Resolved { operation: diff },
        );
        let filtered = super::director_drawer_projection(&ui, &runtime, None);
        assert_eq!(filtered.conversations.len(), 4);
        assert!(
            filtered
                .conversations
                .iter()
                .any(|conversation| conversation.label == "Agent" && conversation.selected)
        );

        let mut controls = LiveTerminalControls::default();
        let mut term = FakeTerminal::default();
        let mut browser = UnavailableBrowserOpener;
        for action in [
            LiveTerminalAction::MoveTabNext,
            LiveTerminalAction::MoveTabPrevious,
        ] {
            assert!(intercept_live_terminal_control(
                &Key::Live(action),
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &mut browser,
                &mut pending_targets,
                20,
                80,
                0,
                0,
            ));
        }
        assert!(!super::select_director_tab(
            &Key::Char('x'),
            &mut ui,
            &mut runtime,
        ));
        assert!(super::select_director_tab(
            &Key::Live(LiveTerminalAction::NextTab),
            &mut ui,
            &mut runtime,
        ));
        assert!(super::select_director_tab(
            &Key::Live(LiveTerminalAction::PreviousTab),
            &mut ui,
            &mut runtime,
        ));
        assert!(mutations.lock().unwrap().iter().any(|mutation| matches!(
            mutation,
            AgentTabIntentMutation::Select {
                session_id: None,
                ..
            }
        )));
    }

    #[test]
    fn director_projection_covers_picker_empty_and_launching_states() {
        let workspace = WorkspaceId::new();
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
        runtime.set_agent_models(
            AvailableModels::new([DefaultModel::Claude, DefaultModel::SakanaAi]),
            DefaultModel::SakanaAi,
        );
        let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
        let _ = runtime.handle_key(Key::Live(LiveTerminalAction::DirectorNew));
        assert_eq!(
            super::director_drawer_projection(&ui, &runtime, None).new,
            super::DirectorNewProjection::Choosing {
                candidates: vec!["claude".to_owned(), "sakana.ai".to_owned()],
                selected: 1,
            }
        );

        let _ = runtime.handle_key(Key::Escape);
        runtime.set_agent_models(AvailableModels::default(), DefaultModel::OpenAi);
        let _ = runtime.handle_key(Key::Live(LiveTerminalAction::DirectorNew));
        assert_eq!(
            super::director_drawer_projection(&ui, &runtime, None).new,
            super::DirectorNewProjection::Empty
        );

        let _ = runtime.handle_key(Key::Escape);
        runtime.set_agent_models(
            AvailableModels::new([DefaultModel::Claude]),
            DefaultModel::Claude,
        );
        let _ = runtime.handle_key(Key::Live(LiveTerminalAction::DirectorNew));
        let effects = runtime.handle_key(Key::Enter);
        assert!(matches!(effects.as_slice(), [Effect::LaunchAgent { .. }]));
        assert_eq!(
            super::director_drawer_projection(&ui, &runtime, None).new,
            super::DirectorNewProjection::Launching
        );
    }

    #[test]
    fn director_tab_cycle_fails_closed_when_intent_cannot_commit() {
        let workspace = WorkspaceId::new();
        let first = scoped_terminal_ref(workspace, None);
        let second = scoped_terminal_ref(workspace, None);
        let first_continuation = AgentContinuationRef::new();
        let second_continuation = AgentContinuationRef::new();
        let mut intent = AgentTabIntent::empty(workspace);
        for (continuation, terminal, select) in [
            (first_continuation, first.clone(), true),
            (second_continuation, second.clone(), false),
        ] {
            intent.apply(AgentTabIntentMutation::Upsert {
                session_id: None,
                continuation,
                terminal,
                select,
            });
        }
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(workspace, Vec::new(), Box::new(UnavailableAgentCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::new(),
                Box::new(FailingIntentPort {
                    state: Arc::new(Mutex::new(intent)),
                    error: AgentTabIntentError::Unavailable,
                    attempts: Arc::new(AtomicUsize::new(0)),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
        let fence = runtime.restore_fence();
        assert!(runtime.restore_snapshot(
            fence.0,
            fence.1,
            vec![super::PaneRestoreTarget {
                target: Target::Root(workspace),
                panes: vec![
                    LivePane {
                        terminal: first.clone(),
                        kind: PaneKind::Agent,
                    },
                    LivePane {
                        terminal: second,
                        kind: PaneKind::Agent,
                    },
                ],
                selected: Some(first.clone()),
                selected_interrupted: None,
                interrupted: Vec::new(),
            }],
        ));
        assert!(!super::select_director_tab(
            &Key::Live(LiveTerminalAction::NextTab),
            &mut ui,
            &mut runtime,
        ));
        let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
        assert!(super::select_director_tab(
            &Key::Live(LiveTerminalAction::NextTab),
            &mut ui,
            &mut runtime,
        ));
        assert_eq!(runtime.focused_terminal(), Some(first));
        assert_eq!(
            runtime
                .state()
                .notice()
                .map(|notice| notice.message.as_str()),
            Some(AgentTabIntentError::Unavailable.safe_message())
        );
    }

    #[test]
    fn director_pointer_uses_the_drawer_viewport() {
        let workspace = WorkspaceId::new();
        let terminal = scoped_terminal_ref(workspace, None);
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(
                workspace,
                Vec::new(),
                Box::new(ScriptedAgentPort {
                    terminal: terminal.clone(),
                    subscription: 919,
                    replay: b"drawer output".to_vec(),
                    poll_error: None,
                    detaches: Arc::new(Mutex::new(Vec::new())),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
        let fence = runtime.restore_fence();
        assert!(runtime.restore_snapshot(
            fence.0,
            fence.1,
            vec![super::PaneRestoreTarget {
                target: Target::Root(workspace),
                panes: vec![LivePane {
                    terminal: terminal.clone(),
                    kind: PaneKind::Agent,
                }],
                selected: Some(terminal.clone()),
                selected_interrupted: None,
                interrupted: Vec::new(),
            }],
        ));
        let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
        ui.start_terminal_session(terminal, foreground_terminal_geometry(20, 80, true));
        let mut controls = LiveTerminalControls::default();
        let mut term = FakeTerminal::default();
        let mut browser = RecordingBrowser::default();

        assert!(handle_terminal_pointer(
            &ui,
            &runtime,
            &mut controls,
            &mut term,
            &mut browser,
            20,
            80,
            1,
            0,
            PointerEvent {
                kind: PointerKind::Down,
                column: 26,
                row: 5,
            },
        ));
    }

    #[test]
    fn root_generic_host_request_and_untracked_resume_completion_are_inert() {
        let workspace = WorkspaceId::new();
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(workspace, Vec::new(), Box::new(UnavailableAgentCommandPort));
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
        let mut pending = std::collections::HashMap::new();
        let (mut host, actions) = ControllerHost::channel();
        super::BackendAgentPort::open_terminal(
            &mut host,
            crate::usecase::application::daemon_backend::OpenTerminalRequest {
                target: Target::Root(workspace),
                operation_id: OperationId::new(),
                arguments: "new".to_owned(),
            },
        );
        drain_host_actions(&actions, &mut ui, &mut runtime, &mut pending);
        assert!(pending.is_empty());
        assert!(ui.pane_launches.is_empty());
        assert_eq!(
            runtime
                .state()
                .notice()
                .map(|notice| notice.message.as_str()),
            Some("♛ Director accepts Agent conversations only")
        );

        ui.pane_completion_sender
            .send(super::PaneLaunchCompletion {
                launch_id: super::PANE_LAUNCH_UNADMITTED,
                outcome: super::PaneLaunchOutcome::ResumeExact {
                    operation: OperationId::new(),
                    continuation: AgentContinuationRef::new(),
                    result: Err("late answer".to_owned()),
                },
            })
            .unwrap();
        super::drain_pane_completions_into_runtime(
            &mut ui,
            &mut runtime,
            &mut pending,
            terminal_geometry(20, 80),
        );
        assert!(runtime.active_pane().tabs().is_empty());
    }

    #[test]
    fn restore_without_agent_intent_caches_inventory_and_refresh_clears_it() {
        let workspace = WorkspaceId::new();
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
        let fence = runtime.restore_fence();
        let applied = super::apply_restore_completion(
            super::RestoreCompletion {
                port: Box::new(UnavailableAgentCommandPort),
                dispatched_interaction: fence.0,
                dispatched_registry_revision: fence.1,
                dispatched_allowed_sessions: BTreeSet::new(),
                terminals: Ok(Vec::new()),
                agents: Ok(AgentInventory {
                    workspace_id: workspace,
                    runtimes: Vec::new(),
                    resumable: Vec::new(),
                }),
                observation_coherent: true,
            },
            &mut ui,
            &mut runtime,
            workspace,
            &BTreeSet::new(),
        );
        assert_eq!(applied.outcome, super::RestoreJobOutcome::Applied);
        assert_eq!(
            ui.agent_inventory().map(|inventory| inventory.workspace_id),
            Some(workspace)
        );
        ui.refresh_agent_inventory();
        assert!(ui.agent_inventory().is_none());
        assert!(ui.take_agent_observation_request());
        assert!(runtime.active_pane().tabs().is_empty());
    }

    #[test]
    fn close_tab_live_action_cancels_the_focused_pending_launch() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let live = live_terminal_ref(workspace, session);
        let (mut ui, mut runtime) = focused_live_pane(
            workspace,
            session,
            live.clone(),
            Box::new(ScriptedAgentPort {
                terminal: live.clone(),
                subscription: 19,
                replay: Vec::new(),
                poll_error: None,
                detaches: Arc::new(Mutex::new(Vec::new())),
            }),
        );
        let operation = OperationId::new();
        let _ = runtime.request_pane(target, operation, PaneKind::Terminal);
        let _ = runtime.select_tab(crate::usecase::application::controller::TabDirection::Next);
        ui.pane_launches.push(PaneLaunch::Terminal {
            operation: OperationId::new(),
            workspace,
            session: Some(session),
            arguments: "open".to_owned(),
        });
        ui.pane_launches.push(PaneLaunch::Agent {
            operation,
            workspace,
            session: Some(session),
            profile: None,
            resume: false,
        });
        let mut pending_targets = std::collections::HashMap::from([(operation, target)]);
        let mut controls = LiveTerminalControls::default();
        let mut term = FakeTerminal::default();
        let mut browser = UnavailableBrowserOpener;

        assert!(intercept_live_terminal_control(
            &Key::Live(LiveTerminalAction::CloseTab),
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut pending_targets,
            20,
            80,
            0,
            0,
        ));

        assert_eq!(runtime.active_pane().tabs().len(), 1);
        assert_eq!(runtime.focused_terminal(), Some(live));
        assert!(!pending_targets.contains_key(&operation));
        assert!(matches!(
            ui.pane_launches.as_slice(),
            [PaneLaunch::Terminal { .. }]
        ));

        let unqueued = OperationId::new();
        let _ = runtime.request_pane(target, unqueued, PaneKind::Terminal);
        let _ = runtime.select_tab(TabDirection::Next);
        pending_targets.insert(unqueued, target);
        super::close_focused_terminal_pane(&mut ui, &mut runtime, &mut pending_targets);
        assert!(!pending_targets.contains_key(&unqueued));
        assert!(matches!(
            ui.pane_launches.as_slice(),
            [PaneLaunch::Terminal { .. }]
        ));

        // Closeup still permits dismissing its only pending tab. Switch blocks
        // the same control before it can reach this pane mutation path.
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut pending_ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
        let mut pending_runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let _ = pending_runtime.handle_key(Key::Down);
        let _ = pending_runtime.handle_key(Key::Enter);
        let operation = OperationId::new();
        let _ = pending_runtime.request_pane(target, operation, PaneKind::Terminal);
        let _ = pending_runtime.select_tab(TabDirection::Next);
        let mut pending_targets = std::collections::HashMap::from([(operation, target)]);
        super::close_focused_terminal_pane(
            &mut pending_ui,
            &mut pending_runtime,
            &mut pending_targets,
        );
        assert!(pending_runtime.active_pane().tabs().is_empty());
        assert!(pending_targets.is_empty());
    }

    /// A daemon inventory double for restore-on-open. It returns a fixed set of
    /// in-scope runtimes and attaches successfully so a restored tab streams.
    type RecordedTerminalInputs = Arc<Mutex<Vec<(TerminalRef, Vec<u8>)>>>;

    struct RestoreInventoryPort {
        entries: Vec<TerminalInventoryEntry>,
        fail: bool,
        inputs: RecordedTerminalInputs,
    }
    #[coverage(off)] // coverage: reason=generic_monomorphization owner=tui expires=2027-01-31 tests=terminal_restore_fake_port_contract
    impl AgentCommandPort for RestoreInventoryPort {
        fn launch(
            &mut self,
            _operation: OperationId,
            _workspace: WorkspaceId,
            _session: Option<SessionId>,
            _profile: Option<AgentProfileId>,
        ) -> Result<AgentPaneAdmission, String> {
            Err("restore never launches".to_owned())
        }
        fn list_terminals(&mut self) -> Result<Vec<TerminalInventoryEntry>, TerminalError> {
            if self.fail {
                Err(TerminalError::Unavailable)
            } else {
                Ok(self.entries.clone())
            }
        }
        fn attach_terminal(
            &mut self,
            _terminal: &TerminalRef,
            geometry: Geometry,
        ) -> Result<TerminalAttach, TerminalError> {
            Ok(TerminalAttach {
                subscription: TerminalSubscription { id: 1, epoch: 1 },
                revision: 1,
                output_offset: 0,
                next_input_seq: None,
                screen: attach_checkpoint(&[], geometry),
                exited: false,
            })
        }
        fn poll_terminal(
            &mut self,
            _terminal: &TerminalRef,
            _after_offset: u64,
        ) -> Result<Vec<TerminalChunk>, TerminalError> {
            Ok(Vec::new())
        }
        fn input_terminal(
            &mut self,
            terminal: &TerminalRef,
            _subscription: TerminalSubscription,
            _input_seq: u64,
            _operation: OperationId,
            bytes: &[u8],
        ) -> Result<TerminalInputOutcome, TerminalError> {
            self.inputs
                .lock()
                .unwrap()
                .push((terminal.clone(), bytes.to_vec()));
            Ok(TerminalInputOutcome::Written)
        }
    }

    struct RetryRestorePort {
        workspace: WorkspaceId,
        entries: Vec<TerminalInventoryEntry>,
        runtimes: Vec<AgentRuntimeInventoryItem>,
        fail_attempts: usize,
        terminal_attempts: Arc<AtomicUsize>,
        agent_attempts: Arc<AtomicUsize>,
    }

    struct SequencedRestorePort {
        terminals: VecDeque<Result<Vec<TerminalInventoryEntry>, TerminalError>>,
        agents: VecDeque<Result<AgentInventory, String>>,
    }

    impl AgentCommandPort for SequencedRestorePort {
        fn launch(
            &mut self,
            _: OperationId,
            _: WorkspaceId,
            _: Option<SessionId>,
            _: Option<AgentProfileId>,
        ) -> Result<AgentPaneAdmission, String> {
            panic!("restore observation must never launch an Agent")
        }

        fn list_terminals(&mut self) -> Result<Vec<TerminalInventoryEntry>, TerminalError> {
            self.terminals
                .pop_front()
                .expect("terminal observation script exhausted")
        }

        fn resume_inventory(&mut self, _: WorkspaceId) -> Result<AgentInventory, String> {
            self.agents
                .pop_front()
                .expect("Agent observation script exhausted")
        }
    }

    impl AgentCommandPort for RetryRestorePort {
        fn launch(
            &mut self,
            _: OperationId,
            _: WorkspaceId,
            _: Option<SessionId>,
            _: Option<AgentProfileId>,
        ) -> Result<AgentPaneAdmission, String> {
            panic!("restore must never launch an Agent")
        }

        fn list_terminals(&mut self) -> Result<Vec<TerminalInventoryEntry>, TerminalError> {
            if self.terminal_attempts.fetch_add(1, Ordering::SeqCst) < self.fail_attempts {
                Err(TerminalError::Unavailable)
            } else {
                Ok(self.entries.clone())
            }
        }

        fn resume_inventory(&mut self, workspace: WorkspaceId) -> Result<AgentInventory, String> {
            assert_eq!(workspace, self.workspace);
            if self.agent_attempts.fetch_add(1, Ordering::SeqCst) < self.fail_attempts {
                Err("temporary inventory failure".to_owned())
            } else {
                Ok(AgentInventory {
                    workspace_id: workspace,
                    runtimes: self.runtimes.clone(),
                    resumable: Vec::new(),
                })
            }
        }
    }

    struct MemoryIntentPort {
        state: Arc<Mutex<AgentTabIntent>>,
        mutations: Arc<Mutex<Vec<AgentTabIntentMutation>>>,
    }

    struct FailingIntentPort {
        state: Arc<Mutex<AgentTabIntent>>,
        error: AgentTabIntentError,
        attempts: Arc<AtomicUsize>,
    }

    struct LoadFailingIntentPort;

    impl AgentTabIntentPort for LoadFailingIntentPort {
        fn load(&mut self, _workspace: WorkspaceId) -> Result<AgentTabIntent, AgentTabIntentError> {
            Err(AgentTabIntentError::ReadOnlySchema)
        }

        fn mutate(
            &mut self,
            _workspace: WorkspaceId,
            _expected_revision: u64,
            _mutation: AgentTabIntentMutation,
        ) -> Result<AgentTabIntentPortCommit, AgentTabIntentError> {
            Err(AgentTabIntentError::ReadOnlySchema)
        }
    }

    impl AgentTabIntentPort for FailingIntentPort {
        fn load(&mut self, workspace: WorkspaceId) -> Result<AgentTabIntent, AgentTabIntentError> {
            let state = self.state.lock().unwrap();
            assert_eq!(workspace, state.workspace_id);
            Ok(state.clone())
        }

        fn mutate(
            &mut self,
            workspace: WorkspaceId,
            _expected_revision: u64,
            _mutation: AgentTabIntentMutation,
        ) -> Result<AgentTabIntentPortCommit, AgentTabIntentError> {
            assert_eq!(workspace, self.state.lock().unwrap().workspace_id);
            self.attempts.fetch_add(1, Ordering::SeqCst);
            Err(self.error)
        }
    }

    impl AgentTabIntentPort for MemoryIntentPort {
        fn load(&mut self, workspace: WorkspaceId) -> Result<AgentTabIntent, AgentTabIntentError> {
            let state = self.state.lock().unwrap();
            assert_eq!(workspace, state.workspace_id);
            Ok(state.clone())
        }

        #[allow(clippy::too_many_lines)] // The fake mirrors the production CAS/causal-close matrix.
        fn mutate(
            &mut self,
            workspace: WorkspaceId,
            expected_revision: u64,
            mutation: AgentTabIntentMutation,
        ) -> Result<AgentTabIntentPortCommit, AgentTabIntentError> {
            let mut state = self.state.lock().unwrap();
            assert_eq!(workspace, state.workspace_id);
            let conflict = expected_revision != state.revision;
            self.mutations.lock().unwrap().push(mutation.clone());
            let before = state.clone();
            let force_close_fence = match &mutation {
                AgentTabIntentMutation::Dismiss { continuation } => {
                    state.targets.iter().any(|target| {
                        target
                            .tabs
                            .iter()
                            .any(|slot| slot.continuation == *continuation)
                    })
                }
                AgentTabIntentMutation::DismissTerminal { terminal }
                | AgentTabIntentMutation::DismissTerminalAndSelect { terminal, .. } => {
                    state.dismisses_terminal(terminal)
                }
                _ => false,
            };
            let mut mutation_applied = true;
            let projection = if conflict {
                match mutation {
                    AgentTabIntentMutation::Observe {
                        terminals,
                        agents,
                        allowed_sessions,
                    }
                    | AgentTabIntentMutation::ObserveAll {
                        terminals,
                        agents,
                        allowed_sessions,
                    } => {
                        mutation_applied = false;
                        Some(state.projected_exact(&terminals, &agents, &allowed_sessions))
                    }
                    AgentTabIntentMutation::Reopen { continuation } => {
                        mutation_applied = !state.dismissed.contains(&continuation);
                        None
                    }
                    AgentTabIntentMutation::Upsert {
                        session_id,
                        continuation,
                        terminal,
                        select,
                    } => {
                        mutation_applied = state.targets.iter().any(|target| {
                            target.session_id == session_id
                                && target.tabs.iter().any(|slot| {
                                    slot.continuation == continuation
                                        && slot.terminal.fences(&terminal)
                                })
                                && (!select || target.selected == Some(continuation))
                                && !state.dismissed.contains(&continuation)
                        });
                        None
                    }
                    AgentTabIntentMutation::Dismiss { continuation } => {
                        state.apply(AgentTabIntentMutation::Dismiss { continuation })
                    }
                    AgentTabIntentMutation::DismissTerminalAndSelect { terminal, .. }
                    | AgentTabIntentMutation::DismissTerminal { terminal } => {
                        state.apply(AgentTabIntentMutation::DismissTerminal { terminal })
                    }
                    AgentTabIntentMutation::Select {
                        session_id,
                        continuation,
                    } => {
                        mutation_applied = state.targets.iter().any(|target| {
                            target.session_id == session_id && target.selected == continuation
                        });
                        None
                    }
                    AgentTabIntentMutation::Reorder {
                        session_id,
                        continuations,
                    } => {
                        mutation_applied = state
                            .targets
                            .iter()
                            .find(|target| target.session_id == session_id)
                            .is_some_and(|target| {
                                target
                                    .tabs
                                    .iter()
                                    .map(|slot| slot.continuation)
                                    .eq(continuations)
                            });
                        None
                    }
                }
            } else {
                match mutation {
                    AgentTabIntentMutation::Upsert {
                        session_id,
                        continuation,
                        terminal,
                        select: _,
                    } if state.dismissed.contains(&continuation) => {
                        mutation_applied = false;
                        state.apply(AgentTabIntentMutation::Upsert {
                            session_id,
                            continuation,
                            terminal,
                            select: false,
                        })
                    }
                    mutation => state.apply(mutation),
                }
            };
            if *state != before || force_close_fence {
                state.revision += 1;
            }
            Ok(AgentTabIntentPortCommit {
                intent: state.clone(),
                projection,
                mutation_applied,
                cas_conflict: conflict,
            })
        }
    }

    #[test]
    fn memory_intent_port_fences_an_idempotent_close_before_reopen() {
        let workspace = WorkspaceId::new();
        let continuation = AgentContinuationRef::new();
        let mut durable = AgentTabIntent::empty(workspace);
        durable.apply(AgentTabIntentMutation::Upsert {
            session_id: None,
            continuation,
            terminal: scoped_terminal_ref(workspace, None),
            select: true,
        });
        durable.revision = 1;
        durable.apply(AgentTabIntentMutation::Dismiss { continuation });
        durable.revision = 2;
        let shared = Arc::new(Mutex::new(durable));
        let mut port = MemoryIntentPort {
            state: Arc::clone(&shared),
            mutations: Arc::new(Mutex::new(Vec::new())),
        };

        let close = port
            .mutate(
                workspace,
                1,
                AgentTabIntentMutation::Dismiss { continuation },
            )
            .unwrap();
        assert!(close.cas_conflict);
        assert_eq!(close.intent.revision, 3);

        let reopen = port
            .mutate(
                workspace,
                2,
                AgentTabIntentMutation::Reopen { continuation },
            )
            .unwrap();
        assert!(reopen.cas_conflict);
        assert!(!reopen.mutation_applied);
        assert!(reopen.intent.dismissed.contains(&continuation));
        assert_eq!(reopen.intent.revision, 3);
        assert_eq!(shared.lock().unwrap().revision, 3);

        // A deferred close merges the same way: the exact fence is recorded
        // under the conflict and a repeat still advances the revision.
        let unobserved = scoped_terminal_ref(workspace, None);
        let deferred = port
            .mutate(
                workspace,
                1,
                AgentTabIntentMutation::DismissTerminalAndSelect {
                    terminal: unobserved.clone(),
                    session_id: None,
                    selected: Some(continuation),
                },
            )
            .unwrap();
        assert!(deferred.cas_conflict);
        assert!(deferred.intent.dismissed_terminals.contains(&unobserved));
        assert_eq!(deferred.intent.revision, 4);
        let repeated = port
            .mutate(
                workspace,
                deferred.intent.revision,
                AgentTabIntentMutation::DismissTerminal {
                    terminal: unobserved,
                },
            )
            .unwrap();
        assert!(!repeated.cas_conflict);
        assert_eq!(repeated.intent.revision, 5);
    }

    #[test]
    fn memory_intent_port_projects_both_stale_observation_variants() {
        let workspace = WorkspaceId::new();
        let mut durable = AgentTabIntent::empty(workspace);
        durable.revision = 1;
        let mut port = MemoryIntentPort {
            state: Arc::new(Mutex::new(durable)),
            mutations: Arc::new(Mutex::new(Vec::new())),
        };
        let inventory = AgentInventory {
            workspace_id: workspace,
            runtimes: Vec::new(),
            resumable: Vec::new(),
        };

        for mutation in [
            AgentTabIntentMutation::Observe {
                terminals: Vec::new(),
                agents: inventory.clone(),
                allowed_sessions: BTreeSet::new(),
            },
            AgentTabIntentMutation::ObserveAll {
                terminals: Vec::new(),
                agents: inventory.clone(),
                allowed_sessions: BTreeSet::new(),
            },
        ] {
            let commit = port.mutate(workspace, 0, mutation).unwrap();
            assert!(commit.cas_conflict);
            assert!(!commit.mutation_applied);
            assert_eq!(commit.projection, Some(AgentTabProjection::default()));
        }
    }

    #[test]
    fn unavailable_and_load_failing_intent_ports_keep_typed_fallback_state() {
        let workspace = WorkspaceId::new();
        let continuation = AgentContinuationRef::new();
        let terminal = scoped_terminal_ref(workspace, None);
        let mut unavailable = super::UnavailableAgentTabIntentPort;
        assert_eq!(
            unavailable.load(workspace).unwrap(),
            AgentTabIntent::empty(workspace)
        );
        let committed = unavailable
            .mutate(
                workspace,
                0,
                AgentTabIntentMutation::Upsert {
                    session_id: None,
                    continuation,
                    terminal,
                    select: true,
                },
            )
            .unwrap();
        assert!(committed.mutation_applied);
        assert!(!committed.cas_conflict);
        assert_eq!(committed.intent.targets[0].selected, Some(continuation));

        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
        let observation = ui
            .observe_agent_tabs(
                Vec::new(),
                AgentInventory {
                    workspace_id: workspace,
                    runtimes: Vec::new(),
                    resumable: Vec::new(),
                },
            )
            .unwrap();
        assert!(observation.cas_accepted);
        assert_eq!(observation.projection, AgentTabProjection::default());
        ui.mutate_agent_intent(AgentTabIntentMutation::Dismiss { continuation })
            .unwrap();

        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_tab_intent(workspace, BTreeSet::new(), Box::new(LoadFailingIntentPort));
        assert_eq!(
            ui.take_agent_tab_intent_load_error(),
            Some(AgentTabIntentError::ReadOnlySchema)
        );
        assert_eq!(ui.take_agent_tab_intent_load_error(), None);
    }

    fn scoped_terminal_ref(workspace: WorkspaceId, session: Option<SessionId>) -> TerminalRef {
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: workspace,
            session_id: session,
            worktree_id: WorktreeId::new(),
        }
    }

    #[test]
    fn restore_worker_retries_both_inventories_without_launching() {
        let workspace = WorkspaceId::new();
        let terminal_attempts = Arc::new(AtomicUsize::new(0));
        let agent_attempts = Arc::new(AtomicUsize::new(0));
        let terminal = scoped_terminal_ref(workspace, None);
        let (sender, receiver) = std::sync::mpsc::channel();

        super::spawn_restore_job(
            Box::new(RetryRestorePort {
                workspace,
                entries: vec![TerminalInventoryEntry {
                    terminal: terminal.clone(),
                    kind: TerminalKind::Terminal,
                    live: true,
                }],
                runtimes: Vec::new(),
                fail_attempts: 2,
                terminal_attempts: Arc::clone(&terminal_attempts),
                agent_attempts: Arc::clone(&agent_attempts),
            }),
            workspace,
            BTreeSet::new(),
            7,
            11,
            sender,
        );

        let completion = receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("bounded restore retry completes");
        assert_eq!(completion.dispatched_interaction, 7);
        assert_eq!(completion.dispatched_registry_revision, 11);
        assert_eq!(completion.terminals.unwrap()[0].terminal, terminal);
        assert_eq!(completion.agents.unwrap().workspace_id, workspace);
        assert!(completion.observation_coherent);
        assert_eq!(terminal_attempts.load(Ordering::SeqCst), 6);
        assert_eq!(agent_attempts.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn restore_worker_retries_a_cross_rpc_snapshot_race_until_refs_are_coherent() {
        let workspace = WorkspaceId::new();
        let continuation = AgentContinuationRef::new();
        let old = scoped_terminal_ref(workspace, None);
        let replacement = scoped_terminal_ref(workspace, None);
        let entry = |terminal: &TerminalRef| TerminalInventoryEntry {
            terminal: terminal.clone(),
            kind: TerminalKind::Agent,
            live: true,
        };
        let inventory = |terminal: &TerminalRef| AgentInventory {
            workspace_id: workspace,
            runtimes: vec![AgentRuntimeInventoryItem {
                runtime: AgentRuntimeRef::new(AgentRuntimeId::new(), terminal.clone(), None)
                    .unwrap(),
                continuation,
                state: AgentRuntimeInventoryState::Live,
                resumed_from: None,
            }],
            resumable: Vec::new(),
        };
        let (sender, receiver) = std::sync::mpsc::channel();
        super::spawn_restore_job(
            Box::new(SequencedRestorePort {
                // First terminal/Agent/terminal bracket races O -> R. The
                // second bracket is stable at R and is the only accepted one.
                terminals: VecDeque::from([
                    Ok(vec![entry(&old)]),
                    Ok(vec![entry(&replacement)]),
                    Ok(vec![entry(&replacement)]),
                    Ok(vec![entry(&replacement)]),
                ]),
                agents: VecDeque::from([Ok(inventory(&old)), Ok(inventory(&replacement))]),
            }),
            workspace,
            BTreeSet::new(),
            0,
            0,
            sender,
        );

        let completion = receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(completion.observation_coherent);
        assert_eq!(
            completion.terminals.as_ref().unwrap()[0].terminal,
            replacement
        );
        assert!(
            completion.agents.as_ref().unwrap().runtimes[0]
                .runtime
                .terminal
                .fences(&replacement)
        );
    }

    #[test]
    fn restore_worker_rejects_an_agent_inventory_from_another_workspace() {
        let workspace = WorkspaceId::new();
        let wrong_inventory = AgentInventory {
            workspace_id: WorkspaceId::new(),
            runtimes: Vec::new(),
            resumable: Vec::new(),
        };
        let (sender, receiver) = std::sync::mpsc::channel();
        super::spawn_restore_job(
            Box::new(SequencedRestorePort {
                terminals: VecDeque::from([
                    Ok(Vec::new()),
                    Ok(Vec::new()),
                    Ok(Vec::new()),
                    Ok(Vec::new()),
                    Ok(Vec::new()),
                    Ok(Vec::new()),
                ]),
                agents: VecDeque::from([
                    Ok(wrong_inventory.clone()),
                    Ok(wrong_inventory.clone()),
                    Ok(wrong_inventory),
                ]),
            }),
            workspace,
            BTreeSet::new(),
            0,
            0,
            sender,
        );

        let completion = receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(!completion.observation_coherent);
        assert_eq!(
            completion.agents.unwrap_err(),
            "Agent inventory scope changed while restoring"
        );
    }

    #[test]
    fn partial_transport_failure_restores_nothing_and_outranks_a_stale_fence() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let generic = scoped_terminal_ref(workspace, Some(session));
        let mut initial_intent = AgentTabIntent::empty(workspace);
        initial_intent.revision = 3;
        let durable = Arc::new(Mutex::new(initial_intent));
        let mutations = Arc::new(Mutex::new(Vec::new()));
        let bytes_before = serde_json::to_vec(&*durable.lock().unwrap()).unwrap();
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::from([session]),
                Box::new(MemoryIntentPort {
                    state: Arc::clone(&durable),
                    mutations: Arc::clone(&mutations),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let dispatched = runtime.restore_fence();
        let runtime_before = runtime.active_pane().clone();
        let partial = super::apply_restore_completion(
            super::RestoreCompletion {
                port: Box::new(UnavailableAgentCommandPort),
                dispatched_interaction: dispatched.0,
                dispatched_registry_revision: dispatched.1,
                dispatched_allowed_sessions: BTreeSet::from([session]),
                terminals: Ok(vec![TerminalInventoryEntry {
                    terminal: generic.clone(),
                    kind: TerminalKind::Terminal,
                    live: true,
                }]),
                agents: Err("Agent inventory unavailable".to_owned()),
                observation_coherent: false,
            },
            &mut ui,
            &mut runtime,
            workspace,
            &BTreeSet::from([session]),
        );
        assert_eq!(partial.outcome, super::RestoreJobOutcome::TransportFailed);
        assert_eq!(runtime.active_pane(), &runtime_before);
        assert_ne!(runtime.focused_terminal(), Some(generic.clone()));
        assert!(mutations.lock().unwrap().is_empty());
        assert_eq!(
            serde_json::to_vec(&*durable.lock().unwrap()).unwrap(),
            bytes_before
        );

        let mut retry = super::RestoreRetryState::new();
        assert!(retry.begin_if_due(std::time::Duration::ZERO));
        assert!(retry.complete(std::time::Duration::ZERO, partial.outcome));
        assert!(!retry.begin_if_due(std::time::Duration::from_millis(249)));
        assert!(retry.begin_if_due(std::time::Duration::from_millis(250)));

        // User activity advances the runtime fence while the next partial
        // request is in flight. Transport failure still wins and advances the
        // outage backoff instead of immediately redispatching.
        let _ = runtime.handle_key(Key::Down);
        let both_failed = super::apply_restore_completion(
            super::RestoreCompletion {
                port: partial.port,
                dispatched_interaction: dispatched.0,
                dispatched_registry_revision: dispatched.1,
                dispatched_allowed_sessions: BTreeSet::from([session]),
                terminals: Ok(vec![TerminalInventoryEntry {
                    terminal: generic,
                    kind: TerminalKind::Terminal,
                    live: true,
                }]),
                agents: Err("Agent inventory unavailable".to_owned()),
                observation_coherent: false,
            },
            &mut ui,
            &mut runtime,
            workspace,
            &BTreeSet::from([session]),
        );
        assert_eq!(
            both_failed.outcome,
            super::RestoreJobOutcome::TransportFailed
        );
        assert!(mutations.lock().unwrap().is_empty());
        assert_eq!(
            serde_json::to_vec(&*durable.lock().unwrap()).unwrap(),
            bytes_before
        );
        assert!(!retry.complete(std::time::Duration::from_millis(250), both_failed.outcome));
        assert!(!retry.begin_if_due(std::time::Duration::from_millis(749)));
        assert!(retry.begin_if_due(std::time::Duration::from_millis(750)));
    }

    #[test]
    fn reconnect_racing_an_in_flight_restore_schedules_one_fresh_observation() {
        let now = std::time::Duration::from_secs(7);
        let mut retry = super::RestoreRetryState::new();
        assert!(retry.begin_if_due(std::time::Duration::ZERO));
        retry.reconnected(1, now);
        assert_eq!(retry.followup, super::RestoreFollowup::Reconnected);
        assert!(!retry.complete(now, super::RestoreJobOutcome::Applied));
        assert_eq!(retry.followup, super::RestoreFollowup::None);
        assert_eq!(retry.failures, 0);
        assert_eq!(retry.next_retry_at, Some(now));
        assert!(retry.begin_if_due(now));
        assert!(!retry.begin_if_due(now));
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One fixture covers scope fencing and duplicate normalization.
    fn restore_scope_change_rejects_snapshot_and_exact_duplicates_normalize_once() {
        let workspace = WorkspaceId::new();
        let original_session = SessionId::new();
        let added_session = SessionId::new();
        let terminal = scoped_terminal_ref(workspace, Some(original_session));
        let entry = TerminalInventoryEntry {
            terminal: terminal.clone(),
            kind: TerminalKind::Terminal,
            live: true,
        };
        let same_terminal_agent = TerminalInventoryEntry {
            terminal: terminal.clone(),
            kind: TerminalKind::Agent,
            live: true,
        };
        let mut duplicated = vec![entry.clone(), same_terminal_agent.clone(), entry.clone()];
        super::normalize_terminal_inventory(&mut duplicated);
        assert_eq!(duplicated, vec![same_terminal_agent, entry.clone()]);
        let generic_only = vec![entry.clone()];
        assert!(super::restore_inventory_is_coherent(
            workspace,
            &BTreeSet::from([original_session]),
            &generic_only,
            &AgentInventory {
                workspace_id: workspace,
                runtimes: Vec::new(),
                resumable: Vec::new(),
            },
        ));
        assert!(!super::restore_inventory_is_coherent(
            workspace,
            &BTreeSet::from([original_session]),
            &generic_only,
            &AgentInventory {
                workspace_id: WorkspaceId::new(),
                runtimes: Vec::new(),
                resumable: Vec::new(),
            },
        ));

        let out_of_scope_terminal = scoped_terminal_ref(workspace, Some(added_session));
        assert!(!super::restore_inventory_is_coherent(
            workspace,
            &BTreeSet::from([original_session]),
            &[TerminalInventoryEntry {
                terminal: out_of_scope_terminal,
                kind: TerminalKind::Terminal,
                live: true,
            }],
            &AgentInventory {
                workspace_id: workspace,
                runtimes: Vec::new(),
                resumable: Vec::new(),
            },
        ));

        let mut conflicting = generic_only.clone();
        conflicting.push(TerminalInventoryEntry {
            terminal: terminal.clone(),
            kind: TerminalKind::Agent,
            live: true,
        });
        assert!(!super::restore_inventory_is_coherent(
            workspace,
            &BTreeSet::from([original_session]),
            &conflicting,
            &AgentInventory {
                workspace_id: workspace,
                runtimes: Vec::new(),
                resumable: Vec::new(),
            },
        ));

        let foreign = scoped_terminal_ref(workspace, Some(added_session));
        let continuation = AgentContinuationRef::new();
        let foreign_runtime = AgentRuntimeInventoryItem {
            runtime: AgentRuntimeRef::new(AgentRuntimeId::new(), foreign, Some(added_session))
                .unwrap(),
            continuation,
            state: AgentRuntimeInventoryState::Live,
            resumed_from: None,
        };
        assert!(!super::restore_inventory_is_coherent(
            workspace,
            &BTreeSet::from([original_session]),
            &generic_only,
            &AgentInventory {
                workspace_id: workspace,
                runtimes: vec![foreign_runtime],
                resumable: Vec::new(),
            },
        ));

        let agent_terminal = scoped_terminal_ref(workspace, Some(original_session));
        let agent_entry = TerminalInventoryEntry {
            terminal: agent_terminal.clone(),
            kind: TerminalKind::Agent,
            live: true,
        };
        let duplicate_runtime = || AgentRuntimeInventoryItem {
            runtime: AgentRuntimeRef::new(
                AgentRuntimeId::new(),
                agent_terminal.clone(),
                Some(original_session),
            )
            .unwrap(),
            continuation,
            state: AgentRuntimeInventoryState::Live,
            resumed_from: None,
        };
        assert!(!super::restore_inventory_is_coherent(
            workspace,
            &BTreeSet::from([original_session]),
            &[agent_entry],
            &AgentInventory {
                workspace_id: workspace,
                runtimes: vec![duplicate_runtime(), duplicate_runtime()],
                resumable: Vec::new(),
            },
        ));

        let view = WorkspaceView::with_runtime_ids(
            ws("demo"),
            state("demo"),
            vec![original_session, added_session],
        );
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
        let mut runtime = WorkspaceRuntime::new(workspace, vec![original_session, added_session]);
        let fence = runtime.restore_fence();
        let applied = super::apply_restore_completion(
            super::RestoreCompletion {
                port: Box::new(UnavailableAgentCommandPort),
                dispatched_interaction: fence.0,
                dispatched_registry_revision: fence.1,
                dispatched_allowed_sessions: BTreeSet::from([original_session]),
                terminals: Ok(vec![entry]),
                agents: Ok(AgentInventory {
                    workspace_id: workspace,
                    runtimes: Vec::new(),
                    resumable: Vec::new(),
                }),
                observation_coherent: true,
            },
            &mut ui,
            &mut runtime,
            workspace,
            &BTreeSet::from([original_session, added_session]),
        );
        assert_eq!(applied.outcome, super::RestoreJobOutcome::FenceRejected);
        assert!(
            runtime
                .panes()
                .pane(Target::Session(original_session))
                .is_some_and(|pane| pane.tabs().is_empty())
        );
        assert_ne!(runtime.focused_terminal(), Some(terminal));
    }

    #[test]
    fn restore_intent_publish_failure_keeps_bytes_but_does_not_block_generic_restore() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let generic = scoped_terminal_ref(workspace, Some(session));
        let durable = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
        let bytes_before = serde_json::to_vec(&*durable.lock().unwrap()).unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::from([session]),
                Box::new(FailingIntentPort {
                    state: Arc::clone(&durable),
                    error: AgentTabIntentError::Unavailable,
                    attempts: Arc::clone(&attempts),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let fence = runtime.restore_fence();
        let applied = super::apply_restore_completion(
            super::RestoreCompletion {
                port: Box::new(UnavailableAgentCommandPort),
                dispatched_interaction: fence.0,
                dispatched_registry_revision: fence.1,
                dispatched_allowed_sessions: BTreeSet::from([session]),
                terminals: Ok(vec![TerminalInventoryEntry {
                    terminal: generic.clone(),
                    kind: TerminalKind::Terminal,
                    live: true,
                }]),
                agents: Ok(AgentInventory {
                    workspace_id: workspace,
                    runtimes: Vec::new(),
                    resumable: Vec::new(),
                }),
                observation_coherent: true,
            },
            &mut ui,
            &mut runtime,
            workspace,
            &BTreeSet::from([session]),
        );

        assert_eq!(
            applied.outcome,
            super::RestoreJobOutcome::IntentFailed(AgentTabIntentError::Unavailable)
        );
        assert!(matches!(
            runtime.active_pane().tabs(),
            [PaneTab::Live(LivePane {
                terminal,
                kind: PaneKind::Terminal
            })] if terminal.fences(&generic)
        ));
        assert_eq!(runtime.focused_terminal(), Some(generic));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(
            serde_json::to_vec(&*durable.lock().unwrap()).unwrap(),
            bytes_before
        );
        let mut retry = super::RestoreRetryState::new();
        assert!(retry.begin_if_due(std::time::Duration::ZERO));
        assert!(!retry.complete(std::time::Duration::ZERO, applied.outcome));
        assert!(!retry.begin_if_due(std::time::Duration::from_secs(60)));
        if let super::RestoreJobOutcome::IntentFailed(error) = applied.outcome {
            super::surface_agent_tab_intent_error(&mut runtime, error);
        }
        assert_eq!(
            runtime
                .state()
                .notice()
                .map(|notice| notice.message.as_str()),
            Some(AgentTabIntentError::Unavailable.safe_message())
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Mixed inventory and prior runtime state share one failure fixture.
    fn mixed_restore_intent_failure_preserves_visible_agents_and_restores_generics() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let continuation = AgentContinuationRef::new();
        let inventory_only_continuation = AgentContinuationRef::new();
        let agent = scoped_terminal_ref(workspace, Some(session));
        let inventory_only_agent = scoped_terminal_ref(workspace, Some(session));
        let existing_generic = scoped_terminal_ref(workspace, Some(session));
        let new_generic = scoped_terminal_ref(workspace, Some(session));
        let mut intent = AgentTabIntent::empty(workspace);
        intent.apply(AgentTabIntentMutation::Upsert {
            session_id: Some(session),
            continuation,
            terminal: agent.clone(),
            select: true,
        });
        intent.revision = 3;
        let durable = Arc::new(Mutex::new(intent));
        let bytes_before = serde_json::to_vec(&*durable.lock().unwrap()).unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::from([session]),
                Box::new(FailingIntentPort {
                    state: Arc::clone(&durable),
                    error: AgentTabIntentError::Unavailable,
                    attempts: Arc::clone(&attempts),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let fence = runtime.restore_fence();
        assert!(runtime.restore_snapshot(
            fence.0,
            fence.1,
            vec![super::PaneRestoreTarget {
                target: Target::Session(session),
                panes: vec![
                    LivePane {
                        terminal: agent.clone(),
                        kind: PaneKind::Agent,
                    },
                    LivePane {
                        terminal: existing_generic.clone(),
                        kind: PaneKind::Terminal,
                    },
                ],
                selected: Some(agent.clone()),
                selected_interrupted: None,
                interrupted: Vec::new(),
            }],
        ));
        let fence = runtime.restore_fence();
        let applied = super::apply_restore_completion(
            super::RestoreCompletion {
                port: Box::new(UnavailableAgentCommandPort),
                dispatched_interaction: fence.0,
                dispatched_registry_revision: fence.1,
                dispatched_allowed_sessions: BTreeSet::from([session]),
                terminals: Ok(vec![
                    TerminalInventoryEntry {
                        terminal: agent.clone(),
                        kind: TerminalKind::Agent,
                        live: true,
                    },
                    TerminalInventoryEntry {
                        terminal: inventory_only_agent.clone(),
                        kind: TerminalKind::Agent,
                        live: true,
                    },
                    TerminalInventoryEntry {
                        terminal: existing_generic.clone(),
                        kind: TerminalKind::Terminal,
                        live: true,
                    },
                    TerminalInventoryEntry {
                        terminal: new_generic.clone(),
                        kind: TerminalKind::Terminal,
                        live: true,
                    },
                ]),
                agents: Ok(AgentInventory {
                    workspace_id: workspace,
                    runtimes: vec![
                        AgentRuntimeInventoryItem {
                            runtime: AgentRuntimeRef::new(
                                AgentRuntimeId::new(),
                                agent.clone(),
                                Some(session),
                            )
                            .unwrap(),
                            continuation,
                            state: AgentRuntimeInventoryState::Live,
                            resumed_from: None,
                        },
                        AgentRuntimeInventoryItem {
                            runtime: AgentRuntimeRef::new(
                                AgentRuntimeId::new(),
                                inventory_only_agent.clone(),
                                Some(session),
                            )
                            .unwrap(),
                            continuation: inventory_only_continuation,
                            state: AgentRuntimeInventoryState::Live,
                            resumed_from: None,
                        },
                    ],
                    resumable: Vec::new(),
                }),
                observation_coherent: true,
            },
            &mut ui,
            &mut runtime,
            workspace,
            &BTreeSet::from([session]),
        );

        assert_eq!(
            applied.outcome,
            super::RestoreJobOutcome::IntentFailed(AgentTabIntentError::Unavailable)
        );
        assert!(matches!(
            runtime.active_pane().tabs(),
            [
                PaneTab::Live(LivePane { terminal: visible_agent, kind: PaneKind::Agent }),
                PaneTab::Live(LivePane { terminal: retained_generic, kind: PaneKind::Terminal }),
                PaneTab::Live(LivePane { terminal: added_generic, kind: PaneKind::Terminal })
            ] if visible_agent.fences(&agent)
                && retained_generic.fences(&existing_generic)
                && added_generic.fences(&new_generic)
        ));
        assert!(runtime.active_pane().tabs().iter().all(|tab| {
            !matches!(tab, PaneTab::Live(pane) if pane.terminal.fences(&inventory_only_agent))
        }));
        assert_eq!(runtime.focused_terminal(), Some(agent));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(
            serde_json::to_vec(&*durable.lock().unwrap()).unwrap(),
            bytes_before
        );
        if let super::RestoreJobOutcome::IntentFailed(error) = applied.outcome {
            super::surface_agent_tab_intent_error(&mut runtime, error);
        }
        assert_eq!(
            runtime
                .state()
                .notice()
                .map(|notice| notice.message.as_str()),
            Some(AgentTabIntentError::Unavailable.safe_message())
        );
    }

    #[test]
    fn restore_retry_backoff_bounds_long_outage_and_reconnect_dispatches_once() {
        let mut retry = super::RestoreRetryState::new();
        let mut jobs = 0_u32;
        let mut rpc_attempts = 0_u32;
        let mut notices = 0_u32;
        let mut frames = 0_u32;
        let end = std::time::Duration::from_secs(60);
        let mut now = std::time::Duration::ZERO;
        while now <= end {
            frames += 1;
            if retry.begin_if_due(now) {
                jobs += 1;
                // One bounded worker attempts a terminal/Agent/terminal
                // consistency bracket three times; ticks add no RPCs.
                rpc_attempts += 9;
                notices +=
                    u32::from(retry.complete(now, super::RestoreJobOutcome::TransportFailed));
            }
            now += std::time::Duration::from_millis(16);
        }
        assert!(frames > 3_000, "the render/input clock stayed live");
        assert!(jobs <= 20, "capped backoff bounded worker churn: {jobs}");
        assert_eq!(rpc_attempts, jobs * 9);
        assert_eq!(notices, 1, "one outage produces one notice");

        retry.reconnected(1, end);
        retry.reconnected(1, end);
        assert!(retry.begin_if_due(end));
        assert!(
            !retry.begin_if_due(end),
            "only one restore can be in flight"
        );
        assert!(!retry.complete(end, super::RestoreJobOutcome::Applied));
        for offset in 1..=1_000 {
            assert!(!retry.begin_if_due(end + std::time::Duration::from_millis(offset)));
        }

        let mut outage = super::RestoreRetryState::new();
        assert!(outage.begin_if_due(std::time::Duration::ZERO));
        assert!(outage.complete(
            std::time::Duration::ZERO,
            super::RestoreJobOutcome::TransportFailed
        ));
        outage.request_observation(std::time::Duration::from_millis(10));
        assert!(
            !outage.begin_if_due(std::time::Duration::from_millis(10)),
            "a local Reopen cannot bypass the outage epoch backoff"
        );
        assert!(outage.begin_if_due(std::time::Duration::from_millis(250)));

        let mut in_flight = super::RestoreRetryState::new();
        assert!(in_flight.begin_if_due(std::time::Duration::ZERO));
        in_flight.request_observation(std::time::Duration::from_millis(1));
        assert!(!in_flight.complete(
            std::time::Duration::from_millis(1),
            super::RestoreJobOutcome::Applied
        ));
        assert!(!in_flight.begin_if_due(std::time::Duration::from_secs(1)));

        let mut changed_idle = super::RestoreRetryState::new();
        assert!(changed_idle.begin_if_due(std::time::Duration::ZERO));
        assert!(
            !changed_idle.complete(std::time::Duration::ZERO, super::RestoreJobOutcome::Applied)
        );
        changed_idle.request_changed_observation(std::time::Duration::from_millis(1));
        assert!(changed_idle.begin_if_due(std::time::Duration::from_millis(1)));

        let mut changed_in_flight = super::RestoreRetryState::new();
        assert!(changed_in_flight.begin_if_due(std::time::Duration::ZERO));
        changed_in_flight.request_changed_observation(std::time::Duration::from_millis(1));
        assert!(!changed_in_flight.complete(
            std::time::Duration::from_millis(1),
            super::RestoreJobOutcome::Applied
        ));
        assert!(
            changed_in_flight.begin_if_due(std::time::Duration::from_millis(1)),
            "an in-flight snapshot may predate an Agent exit and needs one follow-up"
        );
        assert!(!changed_in_flight.begin_if_due(std::time::Duration::from_millis(1)));
    }

    #[test]
    fn failed_restore_keeps_the_port_for_a_reconnect_dispatch() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let (sender, receiver) = std::sync::mpsc::channel();
        super::spawn_restore_job(
            Box::new(RetryRestorePort {
                workspace,
                entries: Vec::new(),
                runtimes: Vec::new(),
                fail_attempts: usize::MAX,
                terminal_attempts: Arc::new(AtomicUsize::new(0)),
                agent_attempts: Arc::new(AtomicUsize::new(0)),
            }),
            workspace,
            BTreeSet::from([session]),
            0,
            0,
            sender,
        );
        let completion = receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);

        let applied = super::apply_restore_completion(
            completion,
            &mut ui,
            &mut runtime,
            workspace,
            &BTreeSet::from([session]),
        );
        assert_eq!(applied.outcome, super::RestoreJobOutcome::TransportFailed);
        let mut retry = super::RestoreRetryState::new();
        assert!(retry.begin_if_due(std::time::Duration::ZERO));
        assert!(retry.complete(
            std::time::Duration::ZERO,
            super::RestoreJobOutcome::TransportFailed
        ));
        assert!(runtime.state().notice().is_none());
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Runtime, durable bytes, and retry fencing share one race fixture.
    fn late_restore_leaves_runtime_and_durable_intent_bytes_unchanged() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let first = AgentContinuationRef::new();
        let second = AgentContinuationRef::new();
        let first_terminal = scoped_terminal_ref(workspace, Some(session));
        let second_terminal = scoped_terminal_ref(workspace, Some(session));
        let mut initial = AgentTabIntent::empty(workspace);
        initial.apply(AgentTabIntentMutation::Upsert {
            session_id: Some(session),
            continuation: first,
            terminal: first_terminal.clone(),
            select: true,
        });
        initial.apply(AgentTabIntentMutation::Upsert {
            session_id: Some(session),
            continuation: second,
            terminal: second_terminal.clone(),
            select: false,
        });
        initial.revision = 4;
        let durable = Arc::new(Mutex::new(initial));
        let mutations = Arc::new(Mutex::new(Vec::new()));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::from([session]),
                Box::new(MemoryIntentPort {
                    state: Arc::clone(&durable),
                    mutations: Arc::clone(&mutations),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let _ = runtime.handle_key(Key::Down);
        let _ = runtime.handle_key(Key::Enter);
        let (dispatched_interaction, dispatched_revision) = runtime.restore_fence();
        assert!(runtime.restore_snapshot(
            dispatched_interaction,
            dispatched_revision,
            vec![super::PaneRestoreTarget {
                target: Target::Session(session),
                panes: vec![
                    LivePane {
                        terminal: first_terminal.clone(),
                        kind: PaneKind::Agent,
                    },
                    LivePane {
                        terminal: second_terminal.clone(),
                        kind: PaneKind::Agent,
                    },
                ],
                selected: Some(first_terminal.clone()),
                selected_interrupted: None,
                interrupted: Vec::new(),
            }],
        ));

        // These are the user changes which make the dispatched observation
        // stale: reorder, select the survivor, then close the former selection.
        let _ = runtime.reorder_tab(TabDirection::Next);
        let _ = runtime.focus_terminal(Target::Session(session), second_terminal.clone());
        let _ = runtime.focus_terminal(Target::Session(session), first_terminal.clone());
        let _ = runtime.close_focused_pane();
        let _ = ui.mutate_agent_intent(AgentTabIntentMutation::Reorder {
            session_id: Some(session),
            continuations: vec![second, first],
        });
        let _ = ui.mutate_agent_intent(AgentTabIntentMutation::Select {
            session_id: Some(session),
            continuation: Some(second),
        });
        let _ = ui.mutate_agent_intent(AgentTabIntentMutation::Dismiss {
            continuation: first,
        });
        let durable_before = serde_json::to_vec(&*durable.lock().unwrap()).unwrap();
        let revision_before = durable.lock().unwrap().revision;
        let runtime_before = runtime.active_pane().clone();
        let mutation_count = mutations.lock().unwrap().len();

        let runtime_item = |continuation, terminal: &TerminalRef| AgentRuntimeInventoryItem {
            runtime: AgentRuntimeRef::new(AgentRuntimeId::new(), terminal.clone(), Some(session))
                .unwrap(),
            continuation,
            state: AgentRuntimeInventoryState::Live,
            resumed_from: None,
        };
        let terminal_inventory = || {
            vec![
                TerminalInventoryEntry {
                    terminal: first_terminal.clone(),
                    kind: TerminalKind::Agent,
                    live: true,
                },
                TerminalInventoryEntry {
                    terminal: second_terminal.clone(),
                    kind: TerminalKind::Agent,
                    live: true,
                },
            ]
        };
        let agent_inventory = || AgentInventory {
            workspace_id: workspace,
            runtimes: vec![
                runtime_item(first, &first_terminal),
                runtime_item(second, &second_terminal),
            ],
            resumable: Vec::new(),
        };
        let mut retry = super::RestoreRetryState::new();
        assert!(retry.begin_if_due(std::time::Duration::ZERO));
        let completion = super::RestoreCompletion {
            port: Box::new(UnavailableAgentCommandPort),
            dispatched_interaction,
            dispatched_registry_revision: dispatched_revision,
            dispatched_allowed_sessions: BTreeSet::from([session]),
            terminals: Ok(terminal_inventory()),
            agents: Ok(agent_inventory()),
            observation_coherent: true,
        };
        let applied = super::apply_restore_completion(
            completion,
            &mut ui,
            &mut runtime,
            workspace,
            &BTreeSet::from([session]),
        );

        assert_eq!(applied.outcome, super::RestoreJobOutcome::FenceRejected);
        assert_eq!(runtime.active_pane(), &runtime_before);
        assert_eq!(mutations.lock().unwrap().len(), mutation_count);
        assert_eq!(durable.lock().unwrap().revision, revision_before);
        assert_eq!(
            serde_json::to_vec(&*durable.lock().unwrap()).unwrap(),
            durable_before
        );

        // A fence rejection is a local UI race, not a daemon outage. Return
        // the dedicated port and admit one observation immediately under the
        // fresh fence, without a notice/backoff or duplicate in-flight job.
        let redispatch_at = std::time::Duration::from_secs(1);
        assert!(!retry.complete(redispatch_at, applied.outcome));
        assert!(retry.begin_if_due(redispatch_at));
        assert!(!retry.begin_if_due(redispatch_at));

        let fresh_fence = runtime.restore_fence();
        let fresh = super::apply_restore_completion(
            super::RestoreCompletion {
                port: applied.port,
                dispatched_interaction: fresh_fence.0,
                dispatched_registry_revision: fresh_fence.1,
                dispatched_allowed_sessions: BTreeSet::from([session]),
                terminals: Ok(terminal_inventory()),
                agents: Ok(agent_inventory()),
                observation_coherent: true,
            },
            &mut ui,
            &mut runtime,
            workspace,
            &BTreeSet::from([session]),
        );
        assert_eq!(fresh.outcome, super::RestoreJobOutcome::Applied);
        assert!(!retry.complete(redispatch_at, fresh.outcome));
        assert_eq!(mutations.lock().unwrap().len(), mutation_count + 1);
        assert_eq!(runtime.focused_terminal(), Some(second_terminal.clone()));
        assert_eq!(runtime.active_pane().tabs().len(), 2);
        assert!(runtime.active_pane().tabs().iter().any(|tab| matches!(
            tab,
            PaneTab::Live(LivePane { terminal, kind: PaneKind::Agent })
                if terminal.fences(&first_terminal)
        )));
        assert!(runtime.active_pane().tabs().iter().any(|tab| matches!(
            tab,
            PaneTab::Live(LivePane { terminal, kind: PaneKind::Agent })
                if terminal.fences(&second_terminal)
        )));
        assert!(!retry.begin_if_due(redispatch_at + std::time::Duration::from_secs(60)));
    }

    #[test]
    #[allow(clippy::too_many_lines)] // The stale and fresh observations must share one durable fixture.
    fn cross_tui_stale_observe_omits_old_ref_then_fresh_observation_restores_replacement() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let continuation = AgentContinuationRef::new();
        let old = scoped_terminal_ref(workspace, Some(session));
        let replacement = scoped_terminal_ref(workspace, Some(session));
        let mut initial = AgentTabIntent::empty(workspace);
        initial.apply(AgentTabIntentMutation::Upsert {
            session_id: Some(session),
            continuation,
            terminal: old.clone(),
            select: true,
        });
        initial.revision = 1;
        let durable = Arc::new(Mutex::new(initial));
        let mutations = Arc::new(Mutex::new(Vec::new()));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::from([session]),
                Box::new(MemoryIntentPort {
                    state: Arc::clone(&durable),
                    mutations: Arc::clone(&mutations),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let dispatched = runtime.restore_fence();

        // Another TUI replaces O with R after this controller loaded revision 1.
        {
            let mut latest = durable.lock().unwrap();
            latest.apply(AgentTabIntentMutation::Upsert {
                session_id: Some(session),
                continuation,
                terminal: replacement.clone(),
                select: true,
            });
            latest.revision += 1;
        }
        let inventory = |terminal: &TerminalRef| AgentInventory {
            workspace_id: workspace,
            runtimes: vec![AgentRuntimeInventoryItem {
                runtime: AgentRuntimeRef::new(
                    AgentRuntimeId::new(),
                    terminal.clone(),
                    Some(session),
                )
                .unwrap(),
                continuation,
                state: AgentRuntimeInventoryState::Live,
                resumed_from: None,
            }],
            resumable: Vec::new(),
        };
        let terminals = |terminal: &TerminalRef| {
            vec![TerminalInventoryEntry {
                terminal: terminal.clone(),
                kind: TerminalKind::Agent,
                live: true,
            }]
        };
        let mut retry = super::RestoreRetryState::new();
        assert!(retry.begin_if_due(std::time::Duration::ZERO));
        let stale = super::apply_restore_completion(
            super::RestoreCompletion {
                port: Box::new(UnavailableAgentCommandPort),
                dispatched_interaction: dispatched.0,
                dispatched_registry_revision: dispatched.1,
                dispatched_allowed_sessions: BTreeSet::from([session]),
                terminals: Ok(terminals(&old)),
                agents: Ok(inventory(&old)),
                observation_coherent: true,
            },
            &mut ui,
            &mut runtime,
            workspace,
            &BTreeSet::from([session]),
        );

        assert_eq!(stale.outcome, super::RestoreJobOutcome::FenceRejected);
        assert!(runtime.active_pane().tabs().is_empty());
        assert_ne!(runtime.focused_terminal(), Some(old));
        assert!(
            durable.lock().unwrap().targets[0].tabs[0]
                .terminal
                .fences(&replacement)
        );
        let redispatch_at = std::time::Duration::from_secs(1);
        assert!(!retry.complete(redispatch_at, stale.outcome));
        assert!(retry.begin_if_due(redispatch_at));
        assert!(!retry.begin_if_due(redispatch_at));

        let fresh_fence = runtime.restore_fence();
        let fresh = super::apply_restore_completion(
            super::RestoreCompletion {
                port: stale.port,
                dispatched_interaction: fresh_fence.0,
                dispatched_registry_revision: fresh_fence.1,
                dispatched_allowed_sessions: BTreeSet::from([session]),
                terminals: Ok(terminals(&replacement)),
                agents: Ok(inventory(&replacement)),
                observation_coherent: true,
            },
            &mut ui,
            &mut runtime,
            workspace,
            &BTreeSet::from([session]),
        );
        assert_eq!(fresh.outcome, super::RestoreJobOutcome::Applied);
        assert!(!retry.complete(redispatch_at, fresh.outcome));
        assert_eq!(runtime.focused_terminal(), Some(replacement));
        assert_eq!(mutations.lock().unwrap().len(), 2);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // This regression keeps the visible stale ref and latest lineage together.
    fn visible_old_ref_can_close_latest_lineage_while_fresh_observation_is_pending() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let continuation = AgentContinuationRef::new();
        let old = scoped_terminal_ref(workspace, Some(session));
        let replacement = scoped_terminal_ref(workspace, Some(session));
        let mut initial = AgentTabIntent::empty(workspace);
        initial.apply(AgentTabIntentMutation::Upsert {
            session_id: Some(session),
            continuation,
            terminal: old.clone(),
            select: true,
        });
        initial.revision = 1;
        let durable = Arc::new(Mutex::new(initial));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::from([session]),
                Box::new(MemoryIntentPort {
                    state: Arc::clone(&durable),
                    mutations: Arc::new(Mutex::new(Vec::new())),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let inventory = |terminal: &TerminalRef| AgentInventory {
            workspace_id: workspace,
            runtimes: vec![AgentRuntimeInventoryItem {
                runtime: AgentRuntimeRef::new(
                    AgentRuntimeId::new(),
                    terminal.clone(),
                    Some(session),
                )
                .unwrap(),
                continuation,
                state: AgentRuntimeInventoryState::Live,
                resumed_from: None,
            }],
            resumable: Vec::new(),
        };
        let terminals = |terminal: &TerminalRef| {
            vec![TerminalInventoryEntry {
                terminal: terminal.clone(),
                kind: TerminalKind::Agent,
                live: true,
            }]
        };
        let completion =
            |terminal: &TerminalRef, fence: (u64, u64), port: Box<dyn AgentCommandPort>| {
                super::RestoreCompletion {
                    port,
                    dispatched_interaction: fence.0,
                    dispatched_registry_revision: fence.1,
                    dispatched_allowed_sessions: BTreeSet::from([session]),
                    terminals: Ok(terminals(terminal)),
                    agents: Ok(inventory(terminal)),
                    observation_coherent: true,
                }
            };

        let first_fence = runtime.restore_fence();
        let first = super::apply_restore_completion(
            completion(&old, first_fence, Box::new(UnavailableAgentCommandPort)),
            &mut ui,
            &mut runtime,
            workspace,
            &BTreeSet::from([session]),
        );
        assert_eq!(first.outcome, super::RestoreJobOutcome::Applied);
        assert_eq!(runtime.focused_terminal(), Some(old.clone()));

        // Another TUI advances this continuation from O to R. The late O
        // observation updates local durable state but must leave the visible O
        // pane untouched until its immediately scheduled fresh observation.
        {
            let mut latest = durable.lock().unwrap();
            latest.apply(AgentTabIntentMutation::Upsert {
                session_id: Some(session),
                continuation,
                terminal: replacement.clone(),
                select: true,
            });
            latest.revision += 1;
        }
        let stale_fence = runtime.restore_fence();
        let stale = super::apply_restore_completion(
            completion(&old, stale_fence, first.port),
            &mut ui,
            &mut runtime,
            workspace,
            &BTreeSet::from([session]),
        );
        assert_eq!(stale.outcome, super::RestoreJobOutcome::FenceRejected);
        assert_eq!(runtime.focused_terminal(), Some(old.clone()));
        assert_eq!(ui.agent_continuation_for(&old), Some(continuation));

        super::close_focused_terminal_pane(
            &mut ui,
            &mut runtime,
            &mut std::collections::HashMap::new(),
        );
        assert_eq!(runtime.focused_terminal(), Some(old.clone()));
        assert!(durable.lock().unwrap().dismissed.is_empty());
        assert!(
            durable.lock().unwrap().targets[0].tabs[0]
                .terminal
                .fences(&replacement)
        );

        let fresh_fence = runtime.restore_fence();
        let fresh = super::apply_restore_completion(
            completion(&replacement, fresh_fence, stale.port),
            &mut ui,
            &mut runtime,
            workspace,
            &BTreeSet::from([session]),
        );
        assert_eq!(fresh.outcome, super::RestoreJobOutcome::Applied);
        assert_eq!(runtime.active_pane().tabs().len(), 1);
        assert_eq!(runtime.focused_terminal(), Some(replacement));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn successful_restore_retains_port_and_reconnect_reobserves_exactly_once() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let continuation = AgentContinuationRef::new();
        let terminal = scoped_terminal_ref(workspace, Some(session));
        let terminal_attempts = Arc::new(AtomicUsize::new(0));
        let agent_attempts = Arc::new(AtomicUsize::new(0));
        let port: Box<dyn AgentCommandPort> = Box::new(RetryRestorePort {
            workspace,
            entries: vec![TerminalInventoryEntry {
                terminal: terminal.clone(),
                kind: TerminalKind::Agent,
                live: true,
            }],
            runtimes: vec![AgentRuntimeInventoryItem {
                runtime: AgentRuntimeRef::new(
                    AgentRuntimeId::new(),
                    terminal.clone(),
                    Some(session),
                )
                .unwrap(),
                continuation,
                state: AgentRuntimeInventoryState::Live,
                resumed_from: None,
            }],
            fail_attempts: 0,
            terminal_attempts: Arc::clone(&terminal_attempts),
            agent_attempts: Arc::clone(&agent_attempts),
        });
        let durable = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
        let mutations = Arc::new(Mutex::new(Vec::new()));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::from([session]),
                Box::new(MemoryIntentPort {
                    state: Arc::clone(&durable),
                    mutations: Arc::clone(&mutations),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut retry = super::RestoreRetryState::new();

        assert!(retry.begin_if_due(std::time::Duration::ZERO));
        let fence = runtime.restore_fence();
        super::spawn_restore_job(
            port,
            workspace,
            BTreeSet::from([session]),
            fence.0,
            fence.1,
            sender.clone(),
        );
        let first = receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        let first = super::apply_restore_completion(
            first,
            &mut ui,
            &mut runtime,
            workspace,
            &BTreeSet::from([session]),
        );
        assert_eq!(first.outcome, super::RestoreJobOutcome::Applied);
        assert!(!retry.complete(std::time::Duration::ZERO, super::RestoreJobOutcome::Applied));
        assert_eq!(mutations.lock().unwrap().len(), 1);
        assert_eq!(runtime.focused_terminal(), Some(terminal.clone()));
        let focus_before = runtime.focused_terminal();
        for tick in 1..=1_000 {
            assert!(!retry.begin_if_due(std::time::Duration::from_millis(tick)));
        }

        // A typed reconnect epoch, not a frame tick, admits one new observation
        // with the same dedicated port. RetryRestorePort::launch panics, so this
        // also proves reconnect inventory never becomes a spawn replay.
        let reconnect_at = std::time::Duration::from_secs(2);
        retry.reconnected(1, reconnect_at);
        retry.reconnected(1, reconnect_at);
        assert!(retry.begin_if_due(reconnect_at));
        assert!(!retry.begin_if_due(reconnect_at));
        let fence = runtime.restore_fence();
        super::spawn_restore_job(
            first.port,
            workspace,
            BTreeSet::from([session]),
            fence.0,
            fence.1,
            sender,
        );
        let second = receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        let second = super::apply_restore_completion(
            second,
            &mut ui,
            &mut runtime,
            workspace,
            &BTreeSet::from([session]),
        );
        assert_eq!(second.outcome, super::RestoreJobOutcome::Applied);
        assert!(!retry.complete(reconnect_at, super::RestoreJobOutcome::Applied));
        assert_eq!(mutations.lock().unwrap().len(), 2);
        assert_eq!(runtime.focused_terminal(), focus_before);
        assert_eq!(terminal_attempts.load(Ordering::SeqCst), 4);
        assert_eq!(agent_attempts.load(Ordering::SeqCst), 2);
        assert!(!retry.begin_if_due(reconnect_at + std::time::Duration::from_secs(60)));
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Lifecycle cleanup, durable state, and retry admission share one fixture.
    fn session_membership_change_requests_one_observation_and_cleans_owned_intent() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let removed_session = SessionId::new();
        let root_open = AgentContinuationRef::new();
        let root_dismissed = AgentContinuationRef::new();
        let removed_selected = AgentContinuationRef::new();
        let removed_dismissed = AgentContinuationRef::new();
        let root_open_terminal = scoped_terminal_ref(workspace, Some(session));
        let root_dismissed_terminal = scoped_terminal_ref(workspace, Some(session));
        let removed_selected_terminal = scoped_terminal_ref(workspace, Some(removed_session));
        let removed_dismissed_terminal = scoped_terminal_ref(workspace, Some(removed_session));
        let mut initial = AgentTabIntent::empty(workspace);
        for (session_id, continuation, terminal, select) in [
            (Some(session), root_open, root_open_terminal.clone(), true),
            (
                Some(session),
                root_dismissed,
                root_dismissed_terminal.clone(),
                false,
            ),
            (
                Some(removed_session),
                removed_selected,
                removed_selected_terminal.clone(),
                true,
            ),
            (
                Some(removed_session),
                removed_dismissed,
                removed_dismissed_terminal.clone(),
                false,
            ),
        ] {
            initial.apply(AgentTabIntentMutation::Upsert {
                session_id,
                continuation,
                terminal,
                select,
            });
        }
        initial.apply(AgentTabIntentMutation::Dismiss {
            continuation: root_dismissed,
        });
        initial.apply(AgentTabIntentMutation::Dismiss {
            continuation: removed_dismissed,
        });
        initial.revision = 9;
        initial.validate(workspace).unwrap();
        let durable = Arc::new(Mutex::new(initial));
        let mutations = Arc::new(Mutex::new(Vec::new()));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::from([session, removed_session]),
                Box::new(MemoryIntentPort {
                    state: Arc::clone(&durable),
                    mutations: Arc::clone(&mutations),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let mut retry = super::RestoreRetryState::new();
        assert!(retry.begin_if_due(std::time::Duration::ZERO));
        let initial_fence = runtime.restore_fence();
        let initial_pairs = [
            (root_open_terminal.clone(), root_open),
            (root_dismissed_terminal, root_dismissed),
            (removed_selected_terminal, removed_selected),
            (removed_dismissed_terminal, removed_dismissed),
        ];
        let initial_restore = super::apply_restore_completion(
            super::RestoreCompletion {
                port: Box::new(UnavailableAgentCommandPort),
                dispatched_interaction: initial_fence.0,
                dispatched_registry_revision: initial_fence.1,
                dispatched_allowed_sessions: BTreeSet::from([session, removed_session]),
                terminals: Ok(initial_pairs
                    .iter()
                    .map(|(terminal, _)| TerminalInventoryEntry {
                        terminal: terminal.clone(),
                        kind: TerminalKind::Agent,
                        live: true,
                    })
                    .collect()),
                agents: Ok(AgentInventory {
                    workspace_id: workspace,
                    runtimes: initial_pairs
                        .iter()
                        .map(|(terminal, continuation)| AgentRuntimeInventoryItem {
                            runtime: AgentRuntimeRef::new(
                                AgentRuntimeId::new(),
                                terminal.clone(),
                                terminal.session_id,
                            )
                            .unwrap(),
                            continuation: *continuation,
                            state: AgentRuntimeInventoryState::Live,
                            resumed_from: None,
                        })
                        .collect(),
                    resumable: Vec::new(),
                }),
                observation_coherent: true,
            },
            &mut ui,
            &mut runtime,
            workspace,
            &BTreeSet::from([session, removed_session]),
        );
        assert_eq!(initial_restore.outcome, super::RestoreJobOutcome::Applied);
        assert!(!retry.complete(std::time::Duration::ZERO, initial_restore.outcome));
        assert_eq!(mutations.lock().unwrap().len(), 1);
        assert!(!ui.take_agent_observation_request());

        ui.set_allowed_agent_sessions(BTreeSet::from([session]));
        assert!(ui.take_agent_observation_request());
        ui.set_allowed_agent_sessions(BTreeSet::from([session]));
        assert!(!ui.take_agent_observation_request());
        let now = std::time::Duration::from_secs(1);
        retry.request_observation(now);
        assert!(retry.begin_if_due(now));
        assert!(!retry.begin_if_due(now));
        let fence = runtime.restore_fence();
        let applied = super::apply_restore_completion(
            super::RestoreCompletion {
                port: Box::new(UnavailableAgentCommandPort),
                dispatched_interaction: fence.0,
                dispatched_registry_revision: fence.1,
                dispatched_allowed_sessions: BTreeSet::from([session]),
                terminals: Ok(vec![TerminalInventoryEntry {
                    terminal: root_open_terminal.clone(),
                    kind: TerminalKind::Agent,
                    live: true,
                }]),
                agents: Ok(AgentInventory {
                    workspace_id: workspace,
                    runtimes: vec![AgentRuntimeInventoryItem {
                        runtime: AgentRuntimeRef::new(
                            AgentRuntimeId::new(),
                            root_open_terminal.clone(),
                            Some(session),
                        )
                        .unwrap(),
                        continuation: root_open,
                        state: AgentRuntimeInventoryState::Live,
                        resumed_from: None,
                    }],
                    resumable: Vec::new(),
                }),
                observation_coherent: true,
            },
            &mut ui,
            &mut runtime,
            workspace,
            &BTreeSet::from([session]),
        );

        assert_eq!(applied.outcome, super::RestoreJobOutcome::Applied);
        assert!(!retry.complete(now, applied.outcome));
        let mutations = mutations.lock().unwrap();
        assert_eq!(mutations.len(), 2);
        assert!(matches!(
            mutations.as_slice(),
            [
                AgentTabIntentMutation::ObserveAll {
                    allowed_sessions: initial_allowed,
                    ..
                },
                AgentTabIntentMutation::ObserveAll {
                    allowed_sessions: removed_allowed,
                    ..
                }
            ] if *initial_allowed == BTreeSet::from([session, removed_session])
                && *removed_allowed == BTreeSet::from([session])
        ));
        drop(mutations);
        let durable = durable.lock().unwrap();
        durable.validate(workspace).unwrap();
        assert!(
            durable
                .targets
                .iter()
                .all(|target| target.session_id != Some(removed_session))
        );
        assert!(durable.dismissed.is_empty());
        assert!(
            durable.targets[0]
                .tabs
                .iter()
                .any(|slot| slot.continuation == root_open)
        );
        assert!(!durable.dismissed.contains(&removed_dismissed));
        assert_eq!(runtime.focused_terminal(), Some(root_open_terminal));
        assert!(!retry.begin_if_due(now + std::time::Duration::from_secs(60)));
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One target matrix fixes Agent ordering and generic deduplication.
    fn reconciled_agent_order_precedes_deterministic_generic_inventory_per_target() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let stale_session = SessionId::new();
        let first = AgentContinuationRef::new();
        let second = AgentContinuationRef::new();
        let first_terminal = scoped_terminal_ref(workspace, None);
        let second_terminal = scoped_terminal_ref(workspace, None);
        let session_agent = scoped_terminal_ref(workspace, Some(session));
        let root_generic = scoped_terminal_ref(workspace, None);
        let session_generic = scoped_terminal_ref(workspace, Some(session));
        let session_generic_second = scoped_terminal_ref(workspace, Some(session));
        let stale_generic = scoped_terminal_ref(workspace, Some(stale_session));
        let projection = AgentTabProjection {
            targets: vec![
                AgentTabTargetProjection {
                    session_id: None,
                    tabs: vec![
                        AgentTabSlotIntent {
                            continuation: second,
                            terminal: second_terminal.clone(),
                        },
                        AgentTabSlotIntent {
                            continuation: first,
                            terminal: first_terminal.clone(),
                        },
                    ],
                    selected: Some(first),
                },
                AgentTabTargetProjection {
                    session_id: Some(session),
                    tabs: vec![AgentTabSlotIntent {
                        continuation: AgentContinuationRef::new(),
                        terminal: session_agent.clone(),
                    }],
                    selected: None,
                },
            ],
        };
        let entries = [
            TerminalInventoryEntry {
                terminal: session_generic.clone(),
                kind: TerminalKind::Terminal,
                live: true,
            },
            TerminalInventoryEntry {
                terminal: root_generic.clone(),
                kind: TerminalKind::Terminal,
                live: true,
            },
            TerminalInventoryEntry {
                terminal: session_generic_second.clone(),
                kind: TerminalKind::Terminal,
                live: true,
            },
            TerminalInventoryEntry {
                terminal: session_generic.clone(),
                kind: TerminalKind::Terminal,
                live: true,
            },
            TerminalInventoryEntry {
                terminal: stale_generic,
                kind: TerminalKind::Terminal,
                live: true,
            },
            TerminalInventoryEntry {
                terminal: first_terminal.clone(),
                kind: TerminalKind::Agent,
                live: true,
            },
            TerminalInventoryEntry {
                terminal: first_terminal.clone(),
                kind: TerminalKind::Terminal,
                live: true,
            },
        ];

        let targets = super::pane_restore_targets(
            workspace,
            &BTreeSet::from([session]),
            projection,
            &entries,
            Some(&session_generic_second),
            Vec::new(),
            &BTreeMap::new(),
        );
        assert_eq!(targets.len(), 2);
        let root = targets
            .iter()
            .find(|target| target.target == Target::Root(workspace))
            .unwrap();
        assert_eq!(root.selected, Some(first_terminal));
        assert_eq!(root.panes[0].terminal, second_terminal);
        assert_eq!(root.panes[1].kind, PaneKind::Agent);
        assert_eq!(root.panes.len(), 2);
        assert!(root.panes.iter().all(|pane| pane.kind == PaneKind::Agent));
        assert!(
            root.panes
                .iter()
                .all(|pane| !pane.terminal.fences(&root_generic))
        );
        let managed = targets
            .iter()
            .find(|target| target.target == Target::Session(session))
            .unwrap();
        assert_eq!(managed.selected, Some(session_generic_second.clone()));
        assert_eq!(managed.panes[0].terminal, session_agent);
        assert!(
            managed
                .panes
                .iter()
                .any(|pane| pane.terminal.fences(&session_generic))
        );
        assert!(
            managed
                .panes
                .iter()
                .any(|pane| pane.terminal.fences(&session_generic_second))
        );
        assert_eq!(
            managed
                .panes
                .iter()
                .filter(|pane| pane.terminal.fences(&session_generic))
                .count(),
            1
        );
    }

    #[test]
    fn coherent_empty_projection_authoritatively_clears_every_scoped_live_target() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let stale = scoped_terminal_ref(workspace, Some(session));
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let fence = runtime.restore_fence();
        assert!(runtime.restore_snapshot(
            fence.0,
            fence.1,
            vec![super::PaneRestoreTarget {
                target: Target::Session(session),
                panes: vec![LivePane {
                    terminal: stale,
                    kind: PaneKind::Agent,
                }],
                selected: None,
                selected_interrupted: None,
                interrupted: Vec::new(),
            }],
        ));
        assert_eq!(
            runtime
                .panes()
                .pane(Target::Session(session))
                .unwrap()
                .tabs()
                .len(),
            1
        );

        let empty = super::pane_restore_targets(
            workspace,
            &BTreeSet::from([session]),
            AgentTabProjection::default(),
            &[],
            None,
            Vec::new(),
            &BTreeMap::new(),
        );
        assert_eq!(empty.len(), 2);
        assert!(empty.iter().all(|target| target.panes.is_empty()));
        let fence = runtime.restore_fence();
        assert!(runtime.restore_snapshot(fence.0, fence.1, empty));
        assert!(
            runtime
                .panes()
                .pane(Target::Session(session))
                .unwrap()
                .tabs()
                .is_empty()
        );
    }

    #[test]
    fn foreground_sync_attaches_only_the_active_selected_tab() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let first = scoped_terminal_ref(workspace, Some(session));
        let second = scoped_terminal_ref(workspace, Some(session));
        let detaches = Arc::new(Mutex::new(Vec::new()));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(
                workspace,
                vec![session],
                Box::new(ScriptedAgentPort {
                    terminal: first.clone(),
                    subscription: 41,
                    replay: b"retained".to_vec(),
                    poll_error: None,
                    detaches: Arc::clone(&detaches),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let _ = runtime.handle_key(Key::Enter);
        let (interaction, revision) = runtime.restore_fence();
        let _ = runtime.restore_snapshot(
            interaction,
            revision,
            vec![super::PaneRestoreTarget {
                target: Target::Session(session),
                panes: vec![
                    LivePane {
                        terminal: first.clone(),
                        kind: PaneKind::Agent,
                    },
                    LivePane {
                        terminal: second.clone(),
                        kind: PaneKind::Agent,
                    },
                ],
                selected: Some(first.clone()),
                selected_interrupted: None,
                interrupted: Vec::new(),
            }],
        );
        let geometry = terminal_geometry(20, 80);

        ui.sync_foreground_terminal(runtime.focused_terminal().as_ref(), geometry);
        // Re-syncing while the same selection is already attached keeps it in
        // place, exercising the fence check that avoids relaunching a live
        // foreground terminal.
        ui.sync_foreground_terminal(runtime.focused_terminal().as_ref(), geometry);
        assert!(ui.terminal_rows(&first, None).is_some());
        assert!(ui.terminal_rows(&second, None).is_none());

        let _ = runtime.focus_terminal(Target::Session(session), second.clone());
        ui.sync_foreground_terminal(runtime.focused_terminal().as_ref(), geometry);
        assert!(ui.terminal_rows(&first, None).is_none());
        assert!(ui.terminal_rows(&second, None).is_some());
        assert_eq!(*detaches.lock().unwrap(), vec![41]);
    }

    #[test]
    fn drawer_round_trip_restores_both_views_and_restates_each_viewport_without_resync() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let managed = scoped_terminal_ref(workspace, Some(session));
        let root = scoped_terminal_ref(workspace, None);
        let calls = Arc::new(Mutex::new(StreamCalls::default()));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(
                workspace,
                vec![session],
                Box::new(RecordingStreamPort(Arc::clone(&calls))),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let _ = runtime.handle_key(Key::Enter);
        let (interaction, revision) = runtime.restore_fence();
        assert!(runtime.restore_snapshot(
            interaction,
            revision,
            vec![
                super::PaneRestoreTarget {
                    target: Target::Root(workspace),
                    panes: vec![LivePane {
                        terminal: root.clone(),
                        kind: PaneKind::Agent,
                    }],
                    selected: Some(root.clone()),
                    selected_interrupted: None,
                    interrupted: Vec::new(),
                },
                super::PaneRestoreTarget {
                    target: Target::Session(session),
                    panes: vec![LivePane {
                        terminal: managed.clone(),
                        kind: PaneKind::Agent,
                    }],
                    selected: Some(managed.clone()),
                    selected_interrupted: None,
                    interrupted: Vec::new(),
                },
            ],
        ));
        let managed_geometry = terminal_geometry(24, 100);
        let drawer_geometry = foreground_terminal_geometry(24, 100, true);
        let mut controls = LiveTerminalControls::default();

        ui.sync_foreground_terminal(Some(&managed), managed_geometry);
        ui.resize_terminals(managed_geometry);
        let _ = controller_terminal_view(&ui, &runtime, &mut controls, 1).unwrap();
        controls.scroll_up();
        controls.begin_selection(TerminalSelection::begin(
            vec!["managed".to_owned()],
            TerminalPoint { row: 0, column: 0 },
        ));
        controls.extend_selection(TerminalPoint { row: 0, column: 6 });
        let _ = controls.finish_drag();

        let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
        assert_eq!(runtime.focused_terminal(), Some(root.clone()));
        ui.sync_foreground_terminal(Some(&root), drawer_geometry);
        ui.resize_terminals(drawer_geometry);
        let retained = ui.retained_terminal_view(&managed, 1).unwrap();
        assert_eq!(retained.rows, vec!["three"]);
        let _ = controller_terminal_view(&ui, &runtime, &mut controls, 1).unwrap();
        controls.scroll_up();
        controls.scroll_up();
        controls.begin_selection(TerminalSelection::begin(
            vec!["drawer".to_owned()],
            TerminalPoint { row: 0, column: 0 },
        ));
        controls.extend_selection(TerminalPoint { row: 0, column: 5 });
        let _ = controls.finish_drag();

        let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
        assert_eq!(runtime.focused_terminal(), Some(managed.clone()));
        ui.sync_foreground_terminal(Some(&managed), managed_geometry);
        ui.resize_terminals(managed_geometry);
        let managed_view = controller_terminal_view(&ui, &runtime, &mut controls, 1).unwrap();
        assert_eq!(managed_view.scroll, 1);
        assert_eq!(
            controls.selection().map(TerminalSelection::text).as_deref(),
            Some("managed")
        );

        let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
        assert_eq!(runtime.focused_terminal(), Some(root.clone()));
        ui.sync_foreground_terminal(Some(&root), drawer_geometry);
        ui.resize_terminals(drawer_geometry);
        let drawer_view = controller_terminal_view(&ui, &runtime, &mut controls, 1).unwrap();
        assert_eq!(drawer_view.scroll, 2);
        assert_eq!(
            controls.selection().map(TerminalSelection::text).as_deref(),
            Some("drawer")
        );

        let calls = calls.lock().unwrap();
        // Every attach states its pane's viewport, including the two that return
        // to a size the pane already had: the daemon released this window's
        // claim on the shared viewport together with the detached attachment.
        // None of it costs a separate resize.
        let round_trip = [(managed, managed_geometry), (root, drawer_geometry)];
        assert_eq!(
            calls.attach_geometries,
            [round_trip.clone(), round_trip].concat()
        );
        assert_eq!(calls.resize_geometries, Vec::new());
        // One attach per focus transition means neither same-geometry reattach
        // entered the checkpoint-refusal retry path.
        assert_eq!(calls.attaches, 4);
        assert_eq!(calls.detaches, 3);
    }

    #[test]
    fn detached_terminal_coordinators_are_bounded_and_evict_the_oldest() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let terminals = (0..=super::DETACHED_TERMINAL_LIMIT)
            .map(|_| scoped_terminal_ref(workspace, Some(session)))
            .collect::<Vec<_>>();
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(
                workspace,
                vec![session],
                Box::new(UnavailableAgentCommandPort),
            );
        let geometry = terminal_geometry(20, 80);

        // Embedders can lose their stream port before teardown. Closing the
        // retained coordinator still removes and retains it without a detach.
        let without_agent = scoped_terminal_ref(workspace, Some(session));
        let mut embedded = WorkspaceUi::new(
            WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]),
            Box::new(UnavailableSessionCommandPort),
        );
        embedded.terminals.push(
            crate::usecase::application::terminal_session::TerminalSession::new(
                without_agent.clone(),
                geometry,
            ),
        );
        embedded.close_terminal(&without_agent);
        assert_eq!(embedded.detached_terminals.len(), 1);

        for terminal in &terminals {
            ui.start_terminal_session(terminal.clone(), geometry);
            ui.close_terminal(terminal);
        }

        assert_eq!(ui.detached_terminals.len(), super::DETACHED_TERMINAL_LIMIT);
        assert!(
            !ui.detached_terminals
                .iter()
                .any(|retained| retained.terminal().fences(&terminals[0]))
        );
        assert!(
            ui.detached_terminals
                .iter()
                .any(|retained| retained.terminal().fences(terminals.last().unwrap()))
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Close and reopen rollback use the same failure fixture.
    fn persistence_failures_leave_close_and_reopen_ui_unchanged_with_typed_notice() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let continuation = AgentContinuationRef::new();
        let terminal = scoped_terminal_ref(workspace, Some(session));
        let mut open_intent = AgentTabIntent::empty(workspace);
        open_intent.apply(AgentTabIntentMutation::Upsert {
            session_id: Some(session),
            continuation,
            terminal: terminal.clone(),
            select: true,
        });
        let durable = Arc::new(Mutex::new(open_intent));
        let attempts = Arc::new(AtomicUsize::new(0));
        let bytes_before = serde_json::to_vec(&*durable.lock().unwrap()).unwrap();
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(
                workspace,
                vec![session],
                Box::new(UnavailableAgentCommandPort),
            )
            .with_agent_tab_intent(
                workspace,
                BTreeSet::from([session]),
                Box::new(FailingIntentPort {
                    state: Arc::clone(&durable),
                    error: AgentTabIntentError::Unavailable,
                    attempts: Arc::clone(&attempts),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let fence = runtime.restore_fence();
        assert!(runtime.restore_snapshot(
            fence.0,
            fence.1,
            vec![super::PaneRestoreTarget {
                target: Target::Session(session),
                panes: vec![LivePane {
                    terminal: terminal.clone(),
                    kind: PaneKind::Agent,
                }],
                selected: Some(terminal.clone()),
                selected_interrupted: None,
                interrupted: Vec::new(),
            }],
        ));

        super::close_focused_terminal_pane(
            &mut ui,
            &mut runtime,
            &mut std::collections::HashMap::new(),
        );

        assert_eq!(runtime.focused_terminal(), Some(terminal));
        assert_eq!(runtime.active_pane().tabs().len(), 1);
        assert_eq!(attempts.load(Ordering::SeqCst), 0);
        assert_eq!(
            serde_json::to_vec(&*durable.lock().unwrap()).unwrap(),
            bytes_before
        );
        assert_eq!(
            runtime
                .state()
                .notice()
                .map(|notice| notice.message.as_str()),
            Some("Agent tabs stay visible; exit the Agent with Ctrl-D")
        );

        let mut closed_intent = durable.lock().unwrap().clone();
        closed_intent.apply(AgentTabIntentMutation::Dismiss { continuation });
        let closed = Arc::new(Mutex::new(closed_intent));
        let closed_bytes = serde_json::to_vec(&*closed.lock().unwrap()).unwrap();
        let reopen_attempts = Arc::new(AtomicUsize::new(0));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(
                workspace,
                vec![session],
                Box::new(UnavailableAgentCommandPort),
            )
            .with_agent_tab_intent(
                workspace,
                BTreeSet::from([session]),
                Box::new(FailingIntentPort {
                    state: Arc::clone(&closed),
                    error: AgentTabIntentError::ReadOnlySchema,
                    attempts: Arc::clone(&reopen_attempts),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(ControllerHostAction::ReopenAgent(ReopenAgentRequest {
                workspace,
                continuation,
            }))
            .unwrap();
        drain_host_actions(
            &receiver,
            &mut ui,
            &mut runtime,
            &mut std::collections::HashMap::new(),
        );

        assert!(runtime.active_pane().tabs().is_empty());
        assert_eq!(reopen_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(
            serde_json::to_vec(&*closed.lock().unwrap()).unwrap(),
            closed_bytes
        );
        assert!(closed.lock().unwrap().dismissed.contains(&continuation));
        assert_eq!(
            runtime
                .state()
                .notice()
                .map(|notice| notice.message.as_str()),
            Some(AgentTabIntentError::ReadOnlySchema.safe_message())
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // The stale-cache regression needs both pane kinds and a fresh observation.
    fn same_tui_reopen_waits_for_fresh_observation_and_preserves_new_generic_pane() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let continuation = AgentContinuationRef::new();
        let agent_terminal = scoped_terminal_ref(workspace, Some(session));
        let generic_terminal = scoped_terminal_ref(workspace, Some(session));
        let durable = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
        let mutations = Arc::new(Mutex::new(Vec::new()));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(
                workspace,
                vec![session],
                Box::new(UnavailableAgentCommandPort),
            )
            .with_agent_tab_intent(
                workspace,
                BTreeSet::from([session]),
                Box::new(MemoryIntentPort {
                    state: Arc::clone(&durable),
                    mutations: Arc::clone(&mutations),
                }),
            );
        // Establish the old empty observation, then admit both panes later in
        // this TUI. Reopen must never rebuild from that obsolete snapshot.
        assert!(
            ui.observe_agent_tabs(
                Vec::new(),
                AgentInventory {
                    workspace_id: workspace,
                    runtimes: Vec::new(),
                    resumable: Vec::new(),
                },
            )
            .unwrap()
            .cas_accepted
        );
        ui.mutate_agent_intent(AgentTabIntentMutation::Upsert {
            session_id: Some(session),
            continuation,
            terminal: agent_terminal.clone(),
            select: true,
        })
        .unwrap();
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let fence = runtime.restore_fence();
        assert!(runtime.restore_snapshot(
            fence.0,
            fence.1,
            vec![super::PaneRestoreTarget {
                target: Target::Session(session),
                panes: vec![
                    LivePane {
                        terminal: agent_terminal.clone(),
                        kind: PaneKind::Agent,
                    },
                    LivePane {
                        terminal: generic_terminal.clone(),
                        kind: PaneKind::Terminal,
                    },
                ],
                selected: Some(agent_terminal.clone()),
                selected_interrupted: None,
                interrupted: Vec::new(),
            }],
        ));
        super::close_focused_terminal_pane(
            &mut ui,
            &mut runtime,
            &mut std::collections::HashMap::new(),
        );
        assert_eq!(runtime.focused_terminal(), Some(agent_terminal.clone()));
        assert!(durable.lock().unwrap().dismissed.is_empty());

        // Seed legacy hidden state to exercise compatibility with an older
        // writer. The current UI itself never creates this state.
        ui.mutate_agent_intent(AgentTabIntentMutation::Dismiss { continuation })
            .unwrap();
        let _ = runtime.close_focused_pane();
        assert_eq!(runtime.focused_terminal(), Some(generic_terminal.clone()));

        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(ControllerHostAction::ReopenAgent(ReopenAgentRequest {
                workspace,
                continuation,
            }))
            .unwrap();
        drain_host_actions(
            &receiver,
            &mut ui,
            &mut runtime,
            &mut std::collections::HashMap::new(),
        );

        assert!(!durable.lock().unwrap().dismissed.contains(&continuation));
        assert_eq!(runtime.focused_terminal(), Some(generic_terminal.clone()));
        assert!(matches!(
            runtime.active_pane().tabs(),
            [PaneTab::Live(LivePane { terminal, kind: PaneKind::Terminal })]
                if terminal.fences(&generic_terminal)
        ));
        assert!(ui.take_agent_observation_request());

        let now = std::time::Duration::from_secs(1);
        let mut retry = super::RestoreRetryState::new();
        assert!(retry.begin_if_due(std::time::Duration::ZERO));
        assert!(!retry.complete(std::time::Duration::ZERO, super::RestoreJobOutcome::Applied));
        retry.request_observation(now);
        assert!(retry.begin_if_due(now));
        assert!(!retry.begin_if_due(now));
        let fence = runtime.restore_fence();
        let applied = super::apply_restore_completion(
            super::RestoreCompletion {
                port: Box::new(UnavailableAgentCommandPort),
                dispatched_interaction: fence.0,
                dispatched_registry_revision: fence.1,
                dispatched_allowed_sessions: BTreeSet::from([session]),
                terminals: Ok(vec![
                    TerminalInventoryEntry {
                        terminal: agent_terminal.clone(),
                        kind: TerminalKind::Agent,
                        live: true,
                    },
                    TerminalInventoryEntry {
                        terminal: generic_terminal.clone(),
                        kind: TerminalKind::Terminal,
                        live: true,
                    },
                ]),
                agents: Ok(AgentInventory {
                    workspace_id: workspace,
                    runtimes: vec![AgentRuntimeInventoryItem {
                        runtime: AgentRuntimeRef::new(
                            AgentRuntimeId::new(),
                            agent_terminal.clone(),
                            Some(session),
                        )
                        .unwrap(),
                        continuation,
                        state: AgentRuntimeInventoryState::Live,
                        resumed_from: None,
                    }],
                    resumable: Vec::new(),
                }),
                observation_coherent: true,
            },
            &mut ui,
            &mut runtime,
            workspace,
            &BTreeSet::from([session]),
        );
        assert_eq!(applied.outcome, super::RestoreJobOutcome::Applied);
        assert!(!retry.complete(now, applied.outcome));
        let restored = runtime
            .active_pane()
            .tabs()
            .iter()
            .filter_map(|tab| match tab {
                PaneTab::Live(pane) => Some(pane.terminal.clone()),
                PaneTab::Pending(_) | PaneTab::Ready(_) | PaneTab::Interrupted(_) => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(restored, vec![agent_terminal, generic_terminal.clone()]);
        assert_eq!(runtime.focused_terminal(), Some(generic_terminal));
        assert_eq!(
            mutations
                .lock()
                .unwrap()
                .iter()
                .filter(|mutation| matches!(mutation, AgentTabIntentMutation::ObserveAll { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn stale_agent_admission_cannot_show_or_focus_a_lineage_closed_by_another_tui() {
        let workspace = WorkspaceId::new();
        let continuation = AgentContinuationRef::new();
        let original = scoped_terminal_ref(workspace, None);
        let replacement = scoped_terminal_ref(workspace, None);
        let mut initial = AgentTabIntent::empty(workspace);
        initial.apply(AgentTabIntentMutation::Upsert {
            session_id: None,
            continuation,
            terminal: original.clone(),
            select: true,
        });
        initial.revision = 1;
        let durable = Arc::new(Mutex::new(initial));
        let mutations = Arc::new(Mutex::new(Vec::new()));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(workspace, Vec::new(), Box::new(UnavailableAgentCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::new(),
                Box::new(MemoryIntentPort {
                    state: Arc::clone(&durable),
                    mutations,
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
        let operation = OperationId::new();
        let target = Target::Root(workspace);
        let _ = runtime.request_pane(target, operation, PaneKind::Agent);
        let mut pending = std::collections::HashMap::from([(operation, target)]);

        // A second writer closes this continuation after the first TUI loaded
        // revision 1 but before its daemon admission returns.
        {
            let mut latest = durable.lock().unwrap();
            let _ = latest.apply(AgentTabIntentMutation::Dismiss { continuation });
            latest.revision += 1;
        }
        ui.pane_completion_sender
            .send(super::PaneLaunchCompletion {
                launch_id: super::PANE_LAUNCH_UNADMITTED,
                outcome: super::PaneLaunchOutcome::Agent {
                    operation,
                    result: Ok(AgentPaneAdmission {
                        terminal: replacement.clone(),
                        continuation: Some(continuation),
                    }),
                },
            })
            .unwrap();

        super::drain_pane_completions_into_runtime(
            &mut ui,
            &mut runtime,
            &mut pending,
            terminal_geometry(20, 80),
        );

        assert!(runtime.active_pane().tabs().is_empty());
        assert_eq!(runtime.focused_terminal(), None);
        assert!(durable.lock().unwrap().dismissed.contains(&continuation));
        assert!(
            durable.lock().unwrap().targets[0].tabs[0]
                .terminal
                .fences(&original)
        );
        assert_eq!(
            runtime
                .state()
                .notice()
                .map(|notice| notice.message.as_str()),
            Some(AgentTabIntentError::ConcurrentChange.safe_message())
        );
    }

    #[test]
    fn closing_selected_agent_keeps_it_visible_without_focus_drift() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let first = AgentContinuationRef::new();
        let closed = AgentContinuationRef::new();
        let first_terminal = scoped_terminal_ref(workspace, Some(session));
        let closed_terminal = scoped_terminal_ref(workspace, Some(session));
        let generic = scoped_terminal_ref(workspace, Some(session));
        let mut intent = AgentTabIntent::empty(workspace);
        intent.apply(AgentTabIntentMutation::Upsert {
            session_id: Some(session),
            continuation: first,
            terminal: first_terminal.clone(),
            select: false,
        });
        intent.apply(AgentTabIntentMutation::Upsert {
            session_id: Some(session),
            continuation: closed,
            terminal: closed_terminal.clone(),
            select: true,
        });
        let durable = Arc::new(Mutex::new(intent));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::from([session]),
                Box::new(MemoryIntentPort {
                    state: Arc::clone(&durable),
                    mutations: Arc::new(Mutex::new(Vec::new())),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let fence = runtime.restore_fence();
        assert!(runtime.restore_snapshot(
            fence.0,
            fence.1,
            vec![super::PaneRestoreTarget {
                target: Target::Session(session),
                panes: vec![
                    LivePane {
                        terminal: first_terminal,
                        kind: PaneKind::Agent,
                    },
                    LivePane {
                        terminal: closed_terminal.clone(),
                        kind: PaneKind::Agent,
                    },
                    LivePane {
                        terminal: generic.clone(),
                        kind: PaneKind::Terminal,
                    },
                ],
                selected: Some(generic.clone()),
                selected_interrupted: None,
                interrupted: Vec::new(),
            }],
        ));
        let _ = runtime.focus_terminal(
            Target::Session(session),
            durable.lock().unwrap().targets[0].tabs[1].terminal.clone(),
        );

        super::close_focused_terminal_pane(
            &mut ui,
            &mut runtime,
            &mut std::collections::HashMap::new(),
        );

        assert_eq!(runtime.focused_terminal(), Some(closed_terminal.clone()));
        {
            let state = durable.lock().unwrap();
            assert!(state.dismissed.is_empty());
            assert_eq!(state.targets[0].selected, Some(closed));
        }

        // Closing a generic tab remains available and is not a conversation
        // dismissal, so it
        // records neither a lineage nor a deferred terminal fence.
        let _ = runtime.focus_terminal(Target::Session(session), generic.clone());
        let before = durable.lock().unwrap().clone();
        super::close_focused_terminal_pane(
            &mut ui,
            &mut runtime,
            &mut std::collections::HashMap::new(),
        );
        assert!(!runtime.active_pane().tabs().iter().any(|tab| matches!(
            tab,
            PaneTab::Live(LivePane { terminal, .. }) if terminal.fences(&generic)
        )));
        assert_eq!(*durable.lock().unwrap(), before);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One fixture closes, replays, reconnects, and reopens.
    fn closing_an_unobserved_live_agent_survives_inventory_replay_and_reconnect() {
        let workspace = WorkspaceId::new();
        let closed_terminal = scoped_terminal_ref(workspace, None);
        let surviving_terminal = scoped_terminal_ref(workspace, None);
        // Nothing is saved yet, so both root conversations are projected from
        // their terminal fence alone (#599) and neither has a continuation.
        let durable = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(workspace, Vec::new(), Box::new(UnavailableAgentCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::new(),
                Box::new(MemoryIntentPort {
                    state: Arc::clone(&durable),
                    mutations: Arc::new(Mutex::new(Vec::new())),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
        // The root target owns the Agent drawer, so it is the active pane only
        // while the drawer is open.
        let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
        let fence = runtime.restore_fence();
        assert!(runtime.restore_snapshot(
            fence.0,
            fence.1,
            vec![super::PaneRestoreTarget {
                target: Target::Root(workspace),
                panes: vec![
                    LivePane {
                        terminal: closed_terminal.clone(),
                        kind: PaneKind::Agent,
                    },
                    LivePane {
                        terminal: surviving_terminal.clone(),
                        kind: PaneKind::Agent,
                    },
                ],
                selected: Some(closed_terminal.clone()),
                selected_interrupted: None,
                interrupted: Vec::new(),
            }],
        ));
        assert_eq!(ui.agent_continuation_for(&closed_terminal), None);

        super::close_focused_terminal_pane(
            &mut ui,
            &mut runtime,
            &mut std::collections::HashMap::new(),
        );

        assert_eq!(runtime.focused_terminal(), Some(closed_terminal));
        assert!(durable.lock().unwrap().dismissed.is_empty());
        assert!(durable.lock().unwrap().dismissed_terminals.is_empty());
        assert_eq!(
            runtime
                .state()
                .notice()
                .map(|notice| notice.message.as_str()),
            Some("Agent tabs stay visible; exit the Agent with Ctrl-D")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Agent and generic routing share one persistence-failure fixture.
    fn persistence_failures_block_agent_reorder_and_selection_but_not_generic_tabs() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let first_terminal = scoped_terminal_ref(workspace, Some(session));
        let second_terminal = scoped_terminal_ref(workspace, Some(session));
        let first = AgentContinuationRef::new();
        let second = AgentContinuationRef::new();
        let mut intent = AgentTabIntent::empty(workspace);
        for (continuation, terminal, select) in [
            (first, first_terminal.clone(), true),
            (second, second_terminal.clone(), false),
        ] {
            intent.apply(AgentTabIntentMutation::Upsert {
                session_id: Some(session),
                continuation,
                terminal,
                select,
            });
        }
        let durable = Arc::new(Mutex::new(intent));
        let bytes_before = serde_json::to_vec(&*durable.lock().unwrap()).unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::from([session]),
                Box::new(FailingIntentPort {
                    state: Arc::clone(&durable),
                    error: AgentTabIntentError::Unavailable,
                    attempts: Arc::clone(&attempts),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let fence = runtime.restore_fence();
        assert!(runtime.restore_snapshot(
            fence.0,
            fence.1,
            vec![super::PaneRestoreTarget {
                target: Target::Session(session),
                panes: vec![
                    LivePane {
                        terminal: first_terminal.clone(),
                        kind: PaneKind::Agent,
                    },
                    LivePane {
                        terminal: second_terminal.clone(),
                        kind: PaneKind::Agent,
                    },
                ],
                selected: Some(first_terminal.clone()),
                selected_interrupted: None,
                interrupted: Vec::new(),
            }],
        ));
        let _ = runtime.handle_key(Key::Enter);
        let tabs_before = runtime.active_pane().tabs().to_vec();
        let mut controls = LiveTerminalControls::default();
        let mut term = FakeTerminal::default();
        let mut browser = UnavailableBrowserOpener;
        assert!(intercept_live_terminal_control(
            &Key::Live(LiveTerminalAction::MoveTabNext),
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut std::collections::HashMap::new(),
            20,
            80,
            0,
            0,
        ));
        assert_eq!(runtime.active_pane().tabs(), tabs_before.as_slice());
        assert!(intercept_live_terminal_control(
            &Key::Live(LiveTerminalAction::MoveTabPrevious),
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut std::collections::HashMap::new(),
            20,
            80,
            0,
            0,
        ));
        assert_eq!(runtime.active_pane().tabs(), tabs_before.as_slice());

        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(ControllerHostAction::SelectTab(TabDirection::Next))
            .unwrap();
        drain_host_actions(
            &receiver,
            &mut ui,
            &mut runtime,
            &mut std::collections::HashMap::new(),
        );
        assert_eq!(runtime.focused_terminal(), Some(first_terminal));
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert_eq!(
            serde_json::to_vec(&*durable.lock().unwrap()).unwrap(),
            bytes_before
        );
        assert_eq!(
            runtime
                .state()
                .notice()
                .map(|notice| notice.message.as_str()),
            Some(AgentTabIntentError::Unavailable.safe_message())
        );

        // A generic-only pane has no Agent intent to persist, so the same
        // unavailable store cannot regress its normal tab controls.
        let generic_first = scoped_terminal_ref(workspace, Some(session));
        let generic_second = scoped_terminal_ref(workspace, Some(session));
        let empty = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
        let generic_attempts = Arc::new(AtomicUsize::new(0));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut generic_ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::from([session]),
                Box::new(FailingIntentPort {
                    state: empty,
                    error: AgentTabIntentError::Unavailable,
                    attempts: Arc::clone(&generic_attempts),
                }),
            );
        let mut generic_runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let fence = generic_runtime.restore_fence();
        assert!(generic_runtime.restore_snapshot(
            fence.0,
            fence.1,
            vec![super::PaneRestoreTarget {
                target: Target::Session(session),
                panes: vec![
                    LivePane {
                        terminal: generic_first.clone(),
                        kind: PaneKind::Terminal,
                    },
                    LivePane {
                        terminal: generic_second.clone(),
                        kind: PaneKind::Terminal,
                    },
                ],
                selected: Some(generic_first),
                selected_interrupted: None,
                interrupted: Vec::new(),
            }],
        ));
        let _ = generic_runtime.handle_key(Key::Enter);
        assert!(intercept_live_terminal_control(
            &Key::Live(LiveTerminalAction::MoveTabNext),
            &mut generic_ui,
            &mut generic_runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut std::collections::HashMap::new(),
            20,
            80,
            0,
            0,
        ));
        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(ControllerHostAction::SelectTab(TabDirection::Next))
            .unwrap();
        drain_host_actions(
            &receiver,
            &mut generic_ui,
            &mut generic_runtime,
            &mut std::collections::HashMap::new(),
        );
        assert_eq!(generic_attempts.load(Ordering::SeqCst), 0);
        assert!(generic_runtime.focused_terminal().is_some());
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Reorder success and both persistence failures share one stable fixture.
    fn reorder_control_commits_agent_lineages_in_the_new_stable_order() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let first_terminal = scoped_terminal_ref(workspace, Some(session));
        let second_terminal = scoped_terminal_ref(workspace, Some(session));
        let first = AgentContinuationRef::new();
        let second = AgentContinuationRef::new();
        let mutations = Arc::new(Mutex::new(Vec::new()));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(
                workspace,
                vec![session],
                Box::new(ScriptedAgentPort {
                    terminal: first_terminal.clone(),
                    subscription: 9,
                    replay: Vec::new(),
                    poll_error: None,
                    detaches: Arc::new(Mutex::new(Vec::new())),
                }),
            )
            .with_agent_tab_intent(
                workspace,
                BTreeSet::from([session]),
                Box::new(MemoryIntentPort {
                    state: Arc::new(Mutex::new(AgentTabIntent::empty(workspace))),
                    mutations: Arc::clone(&mutations),
                }),
            );
        for (continuation, terminal) in [
            (first, first_terminal.clone()),
            (second, second_terminal.clone()),
        ] {
            let _ = ui.mutate_agent_intent(AgentTabIntentMutation::Upsert {
                session_id: Some(session),
                continuation,
                terminal,
                select: false,
            });
        }
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let (interaction, revision) = runtime.restore_fence();
        let _ = runtime.restore_snapshot(
            interaction,
            revision,
            vec![super::PaneRestoreTarget {
                target: Target::Session(session),
                panes: vec![
                    LivePane {
                        terminal: first_terminal.clone(),
                        kind: PaneKind::Agent,
                    },
                    LivePane {
                        terminal: second_terminal,
                        kind: PaneKind::Agent,
                    },
                ],
                selected: Some(first_terminal),
                selected_interrupted: None,
                interrupted: Vec::new(),
            }],
        );
        let _ = runtime.apply_event(AppEvent::Key(AppKey::Enter));
        runtime.on_effect(&Effect::LaunchAgent {
            workspace,
            session: Some(session),
            operation_id: OperationId::new(),
            profile: None,
        });
        let mut controls = LiveTerminalControls::default();
        let mut term = FakeTerminal::default();
        let mut browser = UnavailableBrowserOpener;
        let mut pending = std::collections::HashMap::new();

        assert!(intercept_live_terminal_control(
            &Key::Live(LiveTerminalAction::MoveTabNext),
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut pending,
            20,
            80,
            0,
            0,
        ));
        assert!(matches!(
            mutations.lock().unwrap().last(),
            Some(AgentTabIntentMutation::Reorder {
                session_id: Some(actual),
                continuations,
            }) if *actual == session && continuations == &[second, first]
        ));

        assert!(intercept_live_terminal_control(
            &Key::Live(LiveTerminalAction::MoveTabPrevious),
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut pending,
            20,
            80,
            0,
            0,
        ));
        assert!(matches!(
            mutations.lock().unwrap().last(),
            Some(AgentTabIntentMutation::Reorder {
                session_id: Some(actual),
                continuations,
            }) if *actual == session && continuations == &[first, second]
        ));
    }

    #[test]
    fn restore_open_panes_projects_live_runtimes_and_skips_dead_and_duplicates() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let root_terminal = scoped_terminal_ref(workspace, Some(session));
        let root_agent = scoped_terminal_ref(workspace, Some(session));
        let session_terminal = scoped_terminal_ref(workspace, Some(session));
        let dead = scoped_terminal_ref(workspace, Some(session));
        let entries = vec![
            TerminalInventoryEntry {
                terminal: root_terminal.clone(),
                kind: TerminalKind::Terminal,
                live: true,
            },
            TerminalInventoryEntry {
                terminal: root_agent.clone(),
                kind: TerminalKind::Agent,
                live: true,
            },
            TerminalInventoryEntry {
                terminal: session_terminal.clone(),
                kind: TerminalKind::Terminal,
                live: true,
            },
            // A dead process is reported non-live and must not become a tab.
            TerminalInventoryEntry {
                terminal: dead.clone(),
                kind: TerminalKind::Terminal,
                live: false,
            },
            // A duplicate of a live runtime must not double the tab.
            TerminalInventoryEntry {
                terminal: root_terminal.clone(),
                kind: TerminalKind::Terminal,
                live: true,
            },
        ];
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(
                workspace,
                vec![session],
                Box::new(RestoreInventoryPort {
                    entries,
                    fail: false,
                    inputs: Arc::new(Mutex::new(Vec::new())),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);

        restore_open_panes(&mut ui, &mut runtime, terminal_geometry(20, 80));

        // All three managed-session runtimes are projected, with the duplicate
        // terminal removed.
        assert_eq!(runtime.active_pane().tabs().len(), 3);
        assert!(runtime.state().has_live_pane());
        // Every live runtime is attached and streaming; the dead one is not.
        assert!(ui.terminal_rows(&root_terminal, None).is_some());
        assert!(ui.terminal_rows(&root_agent, None).is_some());
        assert!(ui.terminal_rows(&session_terminal, None).is_some());
        assert!(ui.terminal_rows(&dead, None).is_none());
    }

    #[test]
    fn restored_terminal_and_agent_tabs_deliver_ordinary_closeup_input() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let terminal = scoped_terminal_ref(workspace, Some(session));
        let agent = scoped_terminal_ref(workspace, Some(session));
        let inputs = Arc::new(Mutex::new(Vec::new()));
        let entries = vec![
            TerminalInventoryEntry {
                terminal: terminal.clone(),
                kind: TerminalKind::Terminal,
                live: true,
            },
            TerminalInventoryEntry {
                terminal: agent.clone(),
                kind: TerminalKind::Agent,
                live: true,
            },
        ];
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(
                workspace,
                Vec::new(),
                Box::new(RestoreInventoryPort {
                    entries,
                    fail: false,
                    inputs: inputs.clone(),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let mut controls = LiveTerminalControls::default();
        let mut term = FakeTerminal::default();

        restore_open_panes(&mut ui, &mut runtime, terminal_geometry(20, 80));
        // Inventory restoration stays in Switch, but preselects the first tab so
        // entering Closeup has a concrete input owner instead of a target-only
        // selection hidden behind a non-empty tab strip.
        assert!(!runtime.wants_live_input());
        assert_eq!(runtime.focused_terminal(), Some(terminal.clone()));
        assert!(runtime.handle_key(Key::Enter).is_empty());
        assert!(runtime.wants_live_input());
        assert!(forward_live_terminal_input(
            &mut ui,
            &runtime,
            &mut controls,
            &mut term,
            &Key::Char('x'),
        ));

        let _ = runtime.select_tab(TabDirection::Next);
        assert_eq!(runtime.focused_terminal(), Some(agent.clone()));
        assert!(forward_live_terminal_input(
            &mut ui,
            &runtime,
            &mut controls,
            &mut term,
            &Key::Enter,
        ));
        assert!(forward_live_terminal_input(
            &mut ui,
            &runtime,
            &mut controls,
            &mut term,
            &Key::TerminalCopy {
                fallback: b"copy".to_vec(),
            },
        ));
        assert_eq!(
            *inputs.lock().unwrap(),
            vec![
                (terminal, b"x".to_vec()),
                (agent.clone(), b"\r".to_vec()),
                (agent, b"copy".to_vec()),
            ]
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One production-order fixture observes every reserved and passthrough branch.
    fn production_input_order_reserves_drawer_picker_before_root_agent_pty() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let root_agent = scoped_terminal_ref(workspace, None);
        let inputs = Arc::new(Mutex::new(Vec::new()));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(
                workspace,
                vec![session],
                Box::new(RestoreInventoryPort {
                    entries: vec![TerminalInventoryEntry {
                        terminal: root_agent.clone(),
                        kind: TerminalKind::Agent,
                        live: true,
                    }],
                    fail: false,
                    inputs: Arc::clone(&inputs),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        runtime.set_agent_models(
            AvailableModels::new([DefaultModel::Claude, DefaultModel::OpenAi]),
            DefaultModel::Claude,
        );
        restore_open_panes(&mut ui, &mut runtime, terminal_geometry(20, 80));
        let mut controls = LiveTerminalControls::default();
        let mut term = FakeTerminal::default();

        // This is the same production ordering used by the frame loop: the
        // closed drawer leaves the resolved chord for the reducer, which opens
        // both drawer and picker without sending anything to the managed pane.
        let new = Key::Live(LiveTerminalAction::DirectorNew);
        assert_eq!(
            route_workspace_input_before_reducer(
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &new,
            ),
            WorkspaceInputRoute::Unhandled
        );
        assert!(runtime.handle_key(new).is_empty());
        assert!(runtime.state().director_drawer_open());
        assert!(matches!(
            runtime.state().director_new(),
            DirectorNew::Choosing(DefaultModel::Claude)
        ));
        assert_eq!(runtime.focused_terminal(), Some(root_agent.clone()));

        assert_eq!(
            route_workspace_input_before_reducer(
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &Key::Live(LiveTerminalAction::DirectorNew),
            ),
            WorkspaceInputRoute::Drawer(Vec::new())
        );
        assert_eq!(
            route_workspace_input_before_reducer(
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &Key::Live(LiveTerminalAction::Director),
            ),
            WorkspaceInputRoute::Drawer(Vec::new())
        );
        assert!(!runtime.state().director_drawer_open());
        let reopen = Key::Live(LiveTerminalAction::DirectorNew);
        assert_eq!(
            route_workspace_input_before_reducer(
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &reopen,
            ),
            WorkspaceInputRoute::Unhandled
        );
        assert!(runtime.handle_key(reopen).is_empty());
        for key in [Key::Down, Key::Up] {
            assert_eq!(
                route_workspace_input_before_reducer(
                    &mut ui,
                    &mut runtime,
                    &mut controls,
                    &mut term,
                    &key,
                ),
                WorkspaceInputRoute::Drawer(Vec::new())
            );
        }
        assert_eq!(
            route_workspace_input_before_reducer(
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &Key::Escape,
            ),
            WorkspaceInputRoute::Drawer(Vec::new())
        );
        assert_eq!(runtime.state().director_new(), DirectorNew::Idle);
        assert!(inputs.lock().unwrap().is_empty());

        assert_eq!(
            route_workspace_input_before_reducer(
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &Key::Char('x'),
            ),
            WorkspaceInputRoute::Forwarded
        );
        assert_eq!(
            route_workspace_input_before_reducer(
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &Key::Enter,
            ),
            WorkspaceInputRoute::Forwarded
        );
        assert_eq!(
            *inputs.lock().unwrap(),
            vec![
                (root_agent.clone(), b"x".to_vec()),
                (root_agent.clone(), b"\r".to_vec()),
            ]
        );

        assert_eq!(
            route_workspace_input_before_reducer(
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &Key::Live(LiveTerminalAction::DirectorNew),
            ),
            WorkspaceInputRoute::Drawer(Vec::new())
        );
        let WorkspaceInputRoute::Drawer(effects) = route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &Key::Enter,
        ) else {
            panic!("picker Enter must stay in the drawer");
        };
        assert!(matches!(
            effects.as_slice(),
            [Effect::LaunchAgent {
                session: None,
                profile: Some(profile),
                ..
            }] if profile.as_str() == "claude"
        ));
        assert_eq!(
            route_workspace_input_before_reducer(
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &Key::Escape,
            ),
            WorkspaceInputRoute::Drawer(Vec::new())
        );
        assert!(!runtime.state().director_drawer_open());
        assert_eq!(
            *inputs.lock().unwrap(),
            vec![
                (root_agent.clone(), b"x".to_vec()),
                (root_agent, b"\r".to_vec()),
            ]
        );
    }

    /// `Esc` belongs to the drawer's selected root Agent — an agent CLI reads it
    /// as its own interrupt — so the drawer keeps it only when no live
    /// conversation can receive it. `Ctrl-O Ctrl-G` closes the drawer either way.
    #[test]
    fn drawer_escape_reaches_the_selected_root_agent_and_closes_only_without_one() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let root_agent = scoped_terminal_ref(workspace, None);
        let inputs = Arc::new(Mutex::new(Vec::new()));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(
                workspace,
                vec![session],
                Box::new(RestoreInventoryPort {
                    entries: vec![TerminalInventoryEntry {
                        terminal: root_agent.clone(),
                        kind: TerminalKind::Agent,
                        live: true,
                    }],
                    fail: false,
                    inputs: Arc::clone(&inputs),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        restore_open_panes(&mut ui, &mut runtime, terminal_geometry(20, 80));
        let mut controls = LiveTerminalControls::default();
        let mut term = FakeTerminal::default();

        let open = Key::Live(LiveTerminalAction::Director);
        assert!(runtime.handle_key(open).is_empty());
        assert!(runtime.state().director_drawer_open());
        assert_eq!(runtime.focused_terminal(), Some(root_agent.clone()));

        // The live conversation owns Esc: it reaches the PTY once and the drawer
        // stays open.
        assert_eq!(
            route_workspace_input_before_reducer(
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &Key::Escape,
            ),
            WorkspaceInputRoute::Forwarded
        );
        assert!(runtime.state().director_drawer_open());
        assert_eq!(
            *inputs.lock().unwrap(),
            vec![(root_agent.clone(), vec![0x1b])]
        );

        // Closing stays reachable through the drawer's own chord.
        assert_eq!(
            route_workspace_input_before_reducer(
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &Key::Live(LiveTerminalAction::Director),
            ),
            WorkspaceInputRoute::Drawer(Vec::new())
        );
        assert!(!runtime.state().director_drawer_open());
        assert_eq!(*inputs.lock().unwrap(), vec![(root_agent, vec![0x1b])]);

        // With no conversation to receive it, Esc keeps its drawer meaning.
        let empty_view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut empty_ui = WorkspaceUi::new(empty_view, Box::new(UnavailableSessionCommandPort));
        let mut empty_runtime = WorkspaceRuntime::new(workspace, vec![session]);
        assert!(
            empty_runtime
                .handle_key(Key::Live(LiveTerminalAction::Director))
                .is_empty()
        );
        assert!(empty_runtime.state().director_drawer_open());
        assert_eq!(empty_runtime.focused_terminal(), None);
        assert_eq!(
            route_workspace_input_before_reducer(
                &mut empty_ui,
                &mut empty_runtime,
                &mut controls,
                &mut term,
                &Key::Escape,
            ),
            WorkspaceInputRoute::Drawer(Vec::new())
        );
        assert!(!empty_runtime.state().director_drawer_open());
    }

    /// Director mode gives `Ctrl-O Ctrl-N` to New and moves conversation cycling
    /// to the plain follow-up `Ctrl-O n`. Outside the drawer both chords keep
    /// their managed-pane meaning.
    #[test]
    fn director_mode_retargets_the_new_and_next_tab_chords() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        runtime.set_agent_models(
            AvailableModels::new([DefaultModel::Claude, DefaultModel::OpenAi]),
            DefaultModel::Claude,
        );

        // Closed drawer: every key passes through unchanged.
        for key in [
            Key::Live(LiveTerminalAction::NextTab),
            Key::Live(LiveTerminalAction::DirectorNew),
            Key::Live(LiveTerminalAction::PreviousTab),
            Key::Escape,
        ] {
            assert_eq!(retarget_director_chords(&runtime, key.clone()), key);
        }

        assert!(
            runtime
                .handle_key(Key::Live(LiveTerminalAction::Director))
                .is_empty()
        );
        assert!(runtime.state().director_drawer_open());

        // Open drawer: the two chords swap, and nothing else moves.
        assert_eq!(
            retarget_director_chords(&runtime, Key::Live(LiveTerminalAction::NextTab)),
            Key::Live(LiveTerminalAction::DirectorNew)
        );
        assert_eq!(
            retarget_director_chords(&runtime, Key::Live(LiveTerminalAction::DirectorNew)),
            Key::Live(LiveTerminalAction::NextTab)
        );
        for key in [
            Key::Live(LiveTerminalAction::PreviousTab),
            Key::Live(LiveTerminalAction::Director),
            Key::Escape,
            Key::Char('n'),
        ] {
            assert_eq!(retarget_director_chords(&runtime, key.clone()), key);
        }

        // The retargeted chord reaches the reducer as New, exactly as the frame
        // loop dispatches it.
        let retargeted = retarget_director_chords(&runtime, Key::Live(LiveTerminalAction::NextTab));
        assert!(runtime.handle_key(retargeted).is_empty());
        assert!(matches!(
            runtime.state().director_new(),
            DirectorNew::Choosing(DefaultModel::Claude)
        ));
    }

    #[test]
    #[allow(clippy::too_many_lines)] // The production route matrix intentionally names every input vocabulary variant.
    fn production_route_makes_director_picker_the_exclusive_foreground_owner() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let root_agent = scoped_terminal_ref(workspace, None);
        let inputs = Arc::new(Mutex::new(Vec::new()));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(
                workspace,
                vec![session],
                Box::new(RestoreInventoryPort {
                    entries: vec![TerminalInventoryEntry {
                        terminal: root_agent,
                        kind: TerminalKind::Agent,
                        live: true,
                    }],
                    fail: false,
                    inputs: Arc::clone(&inputs),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        runtime.set_agent_models(
            AvailableModels::new([DefaultModel::Claude, DefaultModel::OpenAi]),
            DefaultModel::Claude,
        );
        restore_open_panes(&mut ui, &mut runtime, terminal_geometry(20, 80));
        let mut controls = LiveTerminalControls::default();
        let mut term = FakeTerminal::default();
        let open_picker = Key::Live(LiveTerminalAction::DirectorNew);
        assert!(runtime.handle_key(open_picker).is_empty());

        let inert_inputs = vec![
            Key::Live(LiveTerminalAction::Switch),
            Key::Live(LiveTerminalAction::OpenCloseupModal),
            Key::Live(LiveTerminalAction::NextTab),
            Key::Live(LiveTerminalAction::PreviousTab),
            Key::Live(LiveTerminalAction::MoveTabNext),
            Key::Live(LiveTerminalAction::MoveTabPrevious),
            Key::Live(LiveTerminalAction::Agent),
            Key::Live(LiveTerminalAction::DirectorNew),
            Key::Live(LiveTerminalAction::CloseTab),
            Key::Live(LiveTerminalAction::ResumeTab),
            Key::Live(LiveTerminalAction::QuitConfirmation),
            Key::Live(LiveTerminalAction::ScrollUp),
            Key::Live(LiveTerminalAction::ScrollDown),
            Key::Passthrough(b"raw".to_vec()),
            Key::Paste("paste".to_owned()),
            Key::TerminalCopy {
                fallback: b"copy".to_vec(),
            },
            Key::Pointer(PointerEvent {
                kind: PointerKind::Down,
                column: 40,
                row: 5,
            }),
            Key::Pointer(PointerEvent {
                kind: PointerKind::Drag,
                column: 41,
                row: 5,
            }),
            Key::Pointer(PointerEvent {
                kind: PointerKind::Up,
                column: 41,
                row: 5,
            }),
            Key::Left,
            Key::Right,
            Key::Home,
            Key::End,
            Key::Delete,
            Key::LineStart,
            Key::LineEnd,
            Key::SelectLeft,
            Key::SelectRight,
            Key::SelectHome,
            Key::SelectEnd,
            Key::Backspace,
            Key::Tab,
            Key::Quit,
            Key::CtrlQ,
            Key::CtrlD,
            Key::Char('x'),
            Key::Click { column: 1, row: 1 },
        ];
        let pane_before = runtime.active_pane().clone();
        for key in &inert_inputs {
            assert_eq!(
                route_workspace_input_before_reducer(
                    &mut ui,
                    &mut runtime,
                    &mut controls,
                    &mut term,
                    key,
                ),
                WorkspaceInputRoute::Drawer(Vec::new()),
                "picker did not own {key:?}"
            );
        }
        assert_eq!(runtime.active_pane(), &pane_before);
        assert!(inputs.lock().unwrap().is_empty());

        // Runtime-only wakeups cross the owner gate. Backend events are drained
        // before this seam by the production loop; Resize/Other are its terminal
        // wake vocabulary and likewise stay downstream.
        for key in [Key::Resize, Key::Other] {
            assert_eq!(
                route_workspace_input_before_reducer(
                    &mut ui,
                    &mut runtime,
                    &mut controls,
                    &mut term,
                    &key,
                ),
                WorkspaceInputRoute::Unhandled,
            );
        }

        // Escape returns Choosing to the drawer conversation. The very next
        // ordinary input is routed to the focused root Agent, with no stale
        // picker ownership carried across events.
        assert_eq!(
            route_workspace_input_before_reducer(
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &Key::Escape,
            ),
            WorkspaceInputRoute::Drawer(Vec::new()),
        );
        assert_eq!(runtime.state().director_new(), DirectorNew::Idle);
        assert_eq!(
            route_workspace_input_before_reducer(
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &Key::Char('z'),
            ),
            WorkspaceInputRoute::Forwarded,
        );
        inputs.lock().unwrap().clear();

        // Empty has the same exclusive ownership even though it has no
        // selectable row and Enter cannot launch anything.
        runtime.set_agent_models(AvailableModels::default(), DefaultModel::Claude);
        assert!(
            runtime
                .handle_key(Key::Live(LiveTerminalAction::DirectorNew))
                .is_empty()
        );
        assert_eq!(runtime.state().director_new(), DirectorNew::Empty);
        for key in inert_inputs
            .iter()
            .chain([Key::Up, Key::Down, Key::Enter].iter())
        {
            assert_eq!(
                route_workspace_input_before_reducer(
                    &mut ui,
                    &mut runtime,
                    &mut controls,
                    &mut term,
                    key,
                ),
                WorkspaceInputRoute::Drawer(Vec::new()),
                "empty projection did not own {key:?}"
            );
        }
        assert!(inputs.lock().unwrap().is_empty());
        assert_eq!(
            route_workspace_input_before_reducer(
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &Key::Escape,
            ),
            WorkspaceInputRoute::Drawer(Vec::new()),
        );
        runtime.set_agent_models(
            AvailableModels::new([DefaultModel::Claude, DefaultModel::OpenAi]),
            DefaultModel::Claude,
        );
        assert!(
            runtime
                .handle_key(Key::Live(LiveTerminalAction::DirectorNew))
                .is_empty()
        );

        // Reserved picker operations remain live. Navigation stays local,
        // Enter emits exactly one launch, and launch-pending remains exclusive.
        for key in [Key::Down, Key::Up] {
            assert_eq!(
                route_workspace_input_before_reducer(
                    &mut ui,
                    &mut runtime,
                    &mut controls,
                    &mut term,
                    &key,
                ),
                WorkspaceInputRoute::Drawer(Vec::new()),
            );
        }
        let WorkspaceInputRoute::Drawer(launch) = route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &Key::Enter,
        ) else {
            panic!("picker Enter was not owned");
        };
        assert!(matches!(
            launch.as_slice(),
            [Effect::LaunchAgent { session: None, .. }]
        ));
        assert!(runtime.state().director_launching().is_some());
        for key in &inert_inputs {
            assert_eq!(
                route_workspace_input_before_reducer(
                    &mut ui,
                    &mut runtime,
                    &mut controls,
                    &mut term,
                    key,
                ),
                WorkspaceInputRoute::Drawer(Vec::new()),
                "launching projection did not own {key:?}"
            );
        }
        assert!(inputs.lock().unwrap().is_empty());

        // The Director chord still closes the foreground owner. The immediately
        // following ordinary input uses the restored downstream PTY route.
        assert_eq!(
            route_workspace_input_before_reducer(
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &Key::Live(LiveTerminalAction::Director),
            ),
            WorkspaceInputRoute::Drawer(Vec::new()),
        );
        assert!(!runtime.state().director_drawer_open());
        assert_eq!(
            route_workspace_input_before_reducer(
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &Key::Char('z'),
            ),
            WorkspaceInputRoute::Unhandled,
        );
    }

    #[test]
    fn double_clicked_append_restored_session_attaches_and_receives_input() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let root_terminal = scoped_terminal_ref(workspace, None);
        let session_terminal = scoped_terminal_ref(workspace, Some(session));
        let inputs = Arc::new(Mutex::new(Vec::new()));
        let entries = vec![
            TerminalInventoryEntry {
                terminal: root_terminal.clone(),
                kind: TerminalKind::Terminal,
                live: true,
            },
            TerminalInventoryEntry {
                terminal: session_terminal.clone(),
                kind: TerminalKind::Agent,
                live: true,
            },
        ];
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(
                workspace,
                vec![session],
                Box::new(RestoreInventoryPort {
                    entries,
                    fail: false,
                    inputs: inputs.clone(),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let (interaction, revision) = runtime.restore_fence();
        assert!(runtime.append_restore_snapshot(
            interaction,
            revision,
            vec![
                super::PaneRestoreTarget {
                    target: Target::Root(workspace),
                    panes: vec![LivePane {
                        terminal: root_terminal,
                        kind: PaneKind::Terminal,
                    }],
                    selected: None,
                    selected_interrupted: None,
                    interrupted: Vec::new(),
                },
                super::PaneRestoreTarget {
                    target: Target::Session(session),
                    panes: vec![LivePane {
                        terminal: session_terminal.clone(),
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
                row: 2,
                at: std::time::Duration::from_millis(at),
            });
        }
        ui.sync_foreground_terminal(
            runtime.focused_terminal().as_ref(),
            terminal_geometry(20, 80),
        );

        let mut controls = LiveTerminalControls::default();
        let mut term = FakeTerminal::default();
        assert!(forward_live_terminal_input(
            &mut ui,
            &runtime,
            &mut controls,
            &mut term,
            &Key::Char('x'),
        ));
        assert_eq!(
            *inputs.lock().unwrap(),
            vec![(session_terminal, b"x".to_vec())]
        );
    }

    #[test]
    fn restore_open_panes_restores_nothing_on_daemon_failure_or_without_a_port() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let live = TerminalInventoryEntry {
            terminal: scoped_terminal_ref(workspace, None),
            kind: TerminalKind::Terminal,
            live: true,
        };

        // A daemon failure restores nothing (and never spawns locally).
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(
                workspace,
                vec![session],
                Box::new(RestoreInventoryPort {
                    entries: vec![live],
                    fail: true,
                    inputs: Arc::new(Mutex::new(Vec::new())),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        restore_open_panes(&mut ui, &mut runtime, terminal_geometry(20, 80));
        assert!(runtime.active_pane().tabs().is_empty());
        assert!(!runtime.state().has_live_pane());

        // An embedder with no Agent port simply finds nothing to restore.
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        restore_open_panes(&mut ui, &mut runtime, terminal_geometry(20, 80));
        assert!(runtime.active_pane().tabs().is_empty());
    }

    #[test]
    fn a_live_terminal_drag_selects_and_release_copies_to_the_clipboard() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let terminal = live_terminal_ref(workspace, session);
        let (mut ui, runtime) = focused_live_pane(
            workspace,
            session,
            terminal.clone(),
            Box::new(ScriptedAgentPort {
                terminal: terminal.clone(),
                subscription: 9,
                replay: b"hello".to_vec(),
                poll_error: None,
                detaches: Arc::new(Mutex::new(Vec::new())),
            }),
        );
        let rows_len = ui
            .terminal_rows(&terminal, None)
            .expect("attached live rows")
            .len();
        let mut term = FakeTerminal::default();
        let mut browser = RecordingBrowser::default();
        let mut controls = LiveTerminalControls::default();
        controls.sync_focus(Some(&terminal));

        // The right pane starts at column 37 (36-wide sidebar + divider) and its
        // content begins at frame row 5. Drag across "hello" and release.
        let drag = |column| PointerEvent {
            kind: PointerKind::Drag,
            column,
            row: 5,
        };
        assert!(handle_terminal_pointer(
            &ui,
            &runtime,
            &mut controls,
            &mut term,
            &mut browser,
            20,
            80,
            rows_len,
            0,
            PointerEvent {
                kind: PointerKind::Down,
                column: 37,
                row: 5,
            },
        ));
        assert!(!controls.has_selection());
        // The next drag report lands at the final "o". The press cell above is
        // still part of the copied range, so this must yield all of "hello".
        handle_terminal_pointer(
            &ui,
            &runtime,
            &mut controls,
            &mut term,
            &mut browser,
            20,
            80,
            rows_len,
            0,
            drag(41),
        );
        handle_terminal_pointer(
            &ui,
            &runtime,
            &mut controls,
            &mut term,
            &mut browser,
            20,
            80,
            rows_len,
            0,
            PointerEvent {
                kind: PointerKind::Up,
                column: 41,
                row: 5,
            },
        );

        assert_eq!(term.copied, vec!["hello".to_owned()]);
        // The completed selection is retained, so the native copy shortcut can
        // copy it again without needing another mouse release.
        assert!(forward_live_terminal_input(
            &mut ui,
            &runtime,
            &mut controls,
            &mut term,
            &Key::TerminalCopy {
                fallback: Vec::new(),
            },
        ));
        assert_eq!(term.copied, vec!["hello".to_owned(), "hello".to_owned()]);
        // Releasing the mouse keeps the range highlighted instead of clearing it,
        // and the projected rows still carry the reverse-video selection.
        assert!(controls.has_selection());
        assert!(!controls.is_dragging());
        let projected = ui
            .terminal_rows(&terminal, controls.selection())
            .expect("selection rows");
        assert!(
            projected.iter().any(|row| row.contains("\u{1b}[7mhello")),
            "selection highlight lost after release: {projected:?}"
        );
        // A drag that copied a selection never also opens a link.
        assert!(browser.opened.is_empty());
    }

    #[test]
    fn retained_terminal_selection_copy_reports_missing_or_empty_selection() {
        let mut term = FakeTerminal::default();
        let mut controls = LiveTerminalControls::default();

        copy_terminal_selection(&mut controls, &mut term);
        assert_eq!(term.copied, Vec::<String>::new());
        assert_eq!(
            controls.project(Vec::new(), 1).feedback.as_deref(),
            Some("no terminal text is selected")
        );

        controls.begin_selection(TerminalSelection::begin(
            vec!["text".to_owned()],
            TerminalPoint { row: 0, column: 4 },
        ));
        copy_terminal_selection(&mut controls, &mut term);
        assert_eq!(term.copied, Vec::<String>::new());
        assert_eq!(
            controls.project(Vec::new(), 1).feedback.as_deref(),
            Some("no terminal text is selected")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // The pointer boundary matrix shares one geometry fixture.
    fn pointer_classifier_covers_inert_scroll_drag_and_click_boundaries() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let terminal = live_terminal_ref(workspace, session);
        let (mut ui, mut runtime) = focused_live_pane(
            workspace,
            session,
            terminal.clone(),
            Box::new(ScriptedAgentPort {
                terminal,
                subscription: 30,
                replay: b"hello".to_vec(),
                poll_error: None,
                detaches: Arc::new(Mutex::new(Vec::new())),
            }),
        );
        let mut controls = LiveTerminalControls::default();
        let mut term = FakeTerminal::default();
        let mut browser = UnavailableBrowserOpener;
        let inactive = WorkspaceRuntime::new(workspace, vec![session]);
        assert!(!forward_live_terminal_input(
            &mut ui,
            &inactive,
            &mut controls,
            &mut term,
            &Key::TerminalCopy {
                fallback: Vec::new(),
            },
        ));
        assert!(forward_live_terminal_input(
            &mut ui,
            &runtime,
            &mut controls,
            &mut term,
            &Key::TerminalCopy {
                fallback: Vec::new(),
            },
        ));
        assert!(forward_live_terminal_input(
            &mut ui,
            &runtime,
            &mut controls,
            &mut term,
            &Key::TerminalCopy {
                fallback: b"fail".to_vec(),
            },
        ));
        assert!(forward_live_terminal_input(
            &mut ui,
            &runtime,
            &mut controls,
            &mut term,
            &Key::Passthrough(b"fail".to_vec()),
        ));
        let _ = poll_and_project_terminals(
            &mut ui,
            &mut runtime,
            &mut controls,
            Geometry { cols: 43, rows: 13 },
        );

        handle_terminal_pointer(
            &ui,
            &inactive,
            &mut controls,
            &mut term,
            &mut browser,
            20,
            80,
            1,
            0,
            PointerEvent {
                kind: PointerKind::Drag,
                column: 40,
                row: 5,
            },
        );
        handle_terminal_pointer(
            &ui,
            &runtime,
            &mut controls,
            &mut term,
            &mut browser,
            20,
            80,
            1,
            0,
            PointerEvent {
                kind: PointerKind::Drag,
                column: 0,
                row: 0,
            },
        );
        handle_terminal_pointer(
            &ui,
            &inactive,
            &mut controls,
            &mut term,
            &mut browser,
            20,
            80,
            1,
            0,
            PointerEvent {
                kind: PointerKind::Up,
                column: 40,
                row: 5,
            },
        );
        handle_terminal_pointer(
            &ui,
            &runtime,
            &mut controls,
            &mut term,
            &mut browser,
            20,
            80,
            1,
            0,
            PointerEvent {
                kind: PointerKind::Up,
                column: 0,
                row: 0,
            },
        );
        // A focus change or an out-of-content release after a valid press
        // consumes the gesture without opening or copying.
        for (release_runtime, column, row) in [(&inactive, 40, 5), (&runtime, 0, 0)] {
            assert!(handle_terminal_pointer(
                &ui,
                &runtime,
                &mut controls,
                &mut term,
                &mut browser,
                20,
                80,
                1,
                0,
                PointerEvent {
                    kind: PointerKind::Down,
                    column: 40,
                    row: 5,
                },
            ));
            assert!(handle_terminal_pointer(
                &ui,
                release_runtime,
                &mut controls,
                &mut term,
                &mut browser,
                20,
                80,
                1,
                0,
                PointerEvent {
                    kind: PointerKind::Up,
                    column,
                    row,
                },
            ));
        }
        for column in [40, 41] {
            handle_terminal_pointer(
                &ui,
                &runtime,
                &mut controls,
                &mut term,
                &mut browser,
                20,
                80,
                1,
                0,
                PointerEvent {
                    kind: PointerKind::Drag,
                    column,
                    row: 5,
                },
            );
        }
        assert!(!handle_terminal_pointer(
            &ui,
            &inactive,
            &mut controls,
            &mut term,
            &mut browser,
            20,
            80,
            1,
            0,
            PointerEvent {
                kind: PointerKind::Down,
                column: 40,
                row: 5,
            },
        ));
        assert!(!handle_terminal_pointer(
            &ui,
            &runtime,
            &mut controls,
            &mut term,
            &mut browser,
            20,
            80,
            1,
            0,
            PointerEvent {
                kind: PointerKind::Down,
                column: 0,
                row: 0,
            },
        ));
        let empty_view = WorkspaceView::with_runtime_ids(ws("empty"), state("empty"), vec![]);
        let empty_ui = WorkspaceUi::new(empty_view, Box::new(UnavailableSessionCommandPort));
        let mut detached_controls = LiveTerminalControls::default();
        detached_controls.sync_focus(runtime.focused_terminal().as_ref());
        detached_controls.press_pointer(TerminalSelection::begin(
            vec!["detached".to_owned()],
            TerminalPoint { row: 0, column: 0 },
        ));
        assert!(handle_terminal_pointer(
            &empty_ui,
            &runtime,
            &mut detached_controls,
            &mut term,
            &mut browser,
            20,
            80,
            1,
            0,
            PointerEvent {
                kind: PointerKind::Up,
                column: 40,
                row: 5,
            },
        ));
        assert!(!handle_terminal_pointer(
            &empty_ui,
            &runtime,
            &mut controls,
            &mut term,
            &mut browser,
            20,
            80,
            1,
            0,
            PointerEvent {
                kind: PointerKind::Down,
                column: 40,
                row: 5,
            },
        ));
        let mut empty_controls = LiveTerminalControls::default();
        for kind in [PointerKind::Drag, PointerKind::Up] {
            handle_terminal_pointer(
                &empty_ui,
                &runtime,
                &mut empty_controls,
                &mut term,
                &mut browser,
                20,
                80,
                1,
                0,
                PointerEvent {
                    kind,
                    column: 40,
                    row: 5,
                },
            );
        }

        let mut pending = std::collections::HashMap::new();
        for key in [
            Key::Live(LiveTerminalAction::ScrollUp),
            Key::Live(LiveTerminalAction::ScrollDown),
            Key::Pointer(PointerEvent {
                kind: PointerKind::Drag,
                column: 0,
                row: 0,
            }),
            Key::Click { column: 0, row: 0 },
        ] {
            let _ = intercept_live_terminal_control(
                &key,
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &mut browser,
                &mut pending,
                20,
                80,
                1,
                0,
            );
        }
    }

    /// A recording [`BrowserOpener`] fake: it captures opened URLs so a pointer
    /// test can assert what (if anything) a click launched, and never runs IO.
    #[derive(Default)]
    struct RecordingBrowser {
        opened: Vec<String>,
    }

    impl BrowserOpener for RecordingBrowser {
        fn open(&mut self, url: &str) -> Result<(), String> {
            self.opened.push(url.to_owned());
            Ok(())
        }
    }

    #[test]
    fn a_down_up_click_on_a_terminal_link_opens_it_without_touching_the_pty() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let terminal = live_terminal_ref(workspace, session);
        let (mut ui, mut runtime) = focused_live_pane(
            workspace,
            session,
            terminal.clone(),
            Box::new(ScriptedAgentPort {
                terminal: terminal.clone(),
                subscription: 11,
                replay: b"see https://example.com/x now".to_vec(),
                poll_error: None,
                detaches: Arc::new(Mutex::new(Vec::new())),
            }),
        );
        let rows_len = ui
            .terminal_rows(&terminal, None)
            .expect("attached live rows")
            .len();
        let mut term = FakeTerminal::default();
        let mut browser = RecordingBrowser::default();
        let mut controls = LiveTerminalControls::default();
        let mut pending_targets = std::collections::HashMap::new();
        controls.sync_focus(Some(&terminal));

        // A press-release with no drag: the URL starts at content column 4, so
        // frame column 37 + 4 = 41 lands on it. Down must not create the
        // one-cell selection that previously stole the release from link-open.
        assert!(intercept_live_terminal_control(
            &Key::Click { column: 41, row: 5 },
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut pending_targets,
            20,
            80,
            rows_len,
            0,
        ));
        assert!(!controls.has_selection());
        assert!(intercept_live_terminal_control(
            &Key::Pointer(PointerEvent {
                kind: PointerKind::Up,
                column: 41,
                row: 5,
            }),
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut pending_targets,
            20,
            80,
            rows_len,
            0,
        ));
        assert_eq!(browser.opened, vec!["https://example.com/x".to_owned()]);
        // A pointer release is not keyboard input, so nothing was forwarded to the
        // child PTY, and the clipboard was left alone.
        assert!(term.copied.is_empty());

        // A complete click on the leading prose (frame column 37 = content
        // column 0) opens nothing.
        assert!(intercept_live_terminal_control(
            &Key::Click { column: 37, row: 5 },
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut pending_targets,
            20,
            80,
            rows_len,
            0,
        ));
        assert!(intercept_live_terminal_control(
            &Key::Pointer(PointerEvent {
                kind: PointerKind::Up,
                column: 37,
                row: 5,
            }),
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut pending_targets,
            20,
            80,
            rows_len,
            0,
        ));
        assert_eq!(browser.opened.len(), 1);
    }

    #[test]
    fn a_terminal_press_waits_for_drag_and_then_anchors_at_its_start_cell() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let terminal = live_terminal_ref(workspace, session);
        let (ui, runtime) = focused_live_pane(
            workspace,
            session,
            terminal.clone(),
            Box::new(ScriptedAgentPort {
                terminal: terminal.clone(),
                subscription: 9,
                replay: b"hello".to_vec(),
                poll_error: None,
                detaches: Arc::new(Mutex::new(Vec::new())),
            }),
        );
        let rows_len = ui
            .terminal_rows(&terminal, None)
            .expect("attached live rows")
            .len();
        let mut controls = LiveTerminalControls::default();
        controls.sync_focus(Some(&terminal));
        // The right pane starts at column 37 and terminal content at row 5. The
        // press anchors the selection at the first "h", before the first drag
        // report reaches the controller.
        let mut term = FakeTerminal::default();
        let mut browser = RecordingBrowser::default();
        assert!(handle_terminal_pointer(
            &ui,
            &runtime,
            &mut controls,
            &mut term,
            &mut browser,
            20,
            80,
            rows_len,
            0,
            PointerEvent {
                kind: PointerKind::Down,
                column: 37,
                row: 5,
            },
        ));
        assert!(!controls.is_dragging());
        assert!(!controls.has_selection());
        handle_terminal_pointer(
            &ui,
            &runtime,
            &mut controls,
            &mut term,
            &mut browser,
            20,
            80,
            rows_len,
            0,
            PointerEvent {
                kind: PointerKind::Drag,
                column: 38,
                row: 5,
            },
        );
        assert!(controls.is_dragging());
        assert_eq!(
            controls.selection().expect("selection started").anchor(),
            TerminalPoint { row: 0, column: 0 }
        );

        // A left-sidebar click remains with sidebar navigation; the terminal
        // interceptor must not consume it.
        assert!(!handle_terminal_pointer(
            &ui,
            &runtime,
            &mut controls,
            &mut term,
            &mut browser,
            20,
            80,
            rows_len,
            0,
            PointerEvent {
                kind: PointerKind::Down,
                column: 5,
                row: 2,
            },
        ));
    }

    #[test]
    fn a_block_selection_over_padding_stays_visible_in_the_projected_rows() {
        // Regression: agents draw space-padded, mostly-blank screens. A block
        // drag across text, a blank line, and trailing padding must reach the
        // projected rows as reverse-video, not be trimmed into an invisible
        // selection (copy already worked from the snapshot cells).
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let terminal = live_terminal_ref(workspace, session);
        let (ui, runtime) = focused_live_pane(
            workspace,
            session,
            terminal.clone(),
            Box::new(ScriptedAgentPort {
                terminal: terminal.clone(),
                subscription: 9,
                replay: b"ab\r\n\r\ncd".to_vec(),
                poll_error: None,
                detaches: Arc::new(Mutex::new(Vec::new())),
            }),
        );
        let cells = ui.terminal_cells(&terminal).expect("attached cells");
        let mut selection = TerminalSelection::begin(cells, TerminalPoint { row: 0, column: 0 });
        selection.extend(TerminalPoint { row: 1, column: 5 });
        let mut controls = LiveTerminalControls::default();
        controls.sync_focus(Some(&terminal));
        controls.begin_selection(selection);
        let rows = controller_terminal_view(&ui, &runtime, &mut controls, 10)
            .expect("selection view")
            .rows;
        // Row 0's trailing padding and the blank row 1 are highlighted.
        assert!(
            rows[0].contains("\u{1b}[7m") && rows[0].contains("ab"),
            "row 0 padding not highlighted: {:?}",
            rows[0]
        );
        assert!(
            rows[1].contains("\u{1b}[7m"),
            "blank row 1 not highlighted: {:?}",
            rows[1]
        );
    }

    #[test]
    fn a_selection_over_long_history_keeps_the_projection_viewport_bounded() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let terminal = live_terminal_ref(workspace, session);
        let replay = (0..1_000)
            .flat_map(|line| format!("line {line}\r\n").into_bytes())
            .collect();
        let (ui, runtime) = focused_live_pane(
            workspace,
            session,
            terminal.clone(),
            Box::new(ScriptedAgentPort {
                terminal: terminal.clone(),
                subscription: 10,
                replay,
                poll_error: None,
                detaches: Arc::new(Mutex::new(Vec::new())),
            }),
        );
        let cells = ui.terminal_cells(&terminal).expect("attached cells");
        let last_row = cells.len().saturating_sub(1);
        let mut selection = TerminalSelection::begin(
            cells,
            TerminalPoint {
                row: last_row.saturating_sub(100),
                column: 0,
            },
        );
        selection.extend(TerminalPoint {
            row: last_row,
            column: 2,
        });
        let mut controls = LiveTerminalControls::default();
        controls.sync_focus(Some(&terminal));
        controls.begin_selection(selection);

        let viewport_rows = 20;
        let view = controller_terminal_view(&ui, &runtime, &mut controls, viewport_rows)
            .expect("selection view");
        assert!(view.total_rows > viewport_rows);
        assert_eq!(view.rows.len(), viewport_rows);
        assert_eq!(view.row_offset + view.rows.len(), view.total_rows);

        controls.scroll_up();
        let scrolled = controller_terminal_view(&ui, &runtime, &mut controls, viewport_rows)
            .expect("scrolled selection view");
        assert_eq!(scrolled.rows.len(), viewport_rows);
        assert_eq!(scrolled.scroll, 1);
        assert_eq!(
            scrolled.row_offset + scrolled.rows.len() + scrolled.scroll,
            scrolled.total_rows
        );
    }

    #[test]
    fn scrolling_a_live_terminal_offsets_its_projected_viewport() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let terminal = live_terminal_ref(workspace, session);
        // Enough output to overflow the viewport so scrolling has headroom.
        let replay: Vec<u8> = (0..40)
            .flat_map(|line| format!("line {line}\r\n").into_bytes())
            .collect();
        let (ui, runtime) = focused_live_pane(
            workspace,
            session,
            terminal.clone(),
            Box::new(ScriptedAgentPort {
                terminal: terminal.clone(),
                subscription: 3,
                replay,
                poll_error: None,
                detaches: Arc::new(Mutex::new(Vec::new())),
            }),
        );
        let mut controls = LiveTerminalControls::default();
        let viewport_rows = usize::from(terminal_geometry(20, 80).rows);

        // The first projection anchors at the live bottom (scroll 0).
        let live_bottom = controller_terminal_view(&ui, &runtime, &mut controls, viewport_rows)
            .expect("live view");
        assert_eq!(live_bottom.scroll, 0);
        assert!(live_bottom.total_rows > live_bottom.rows.len());
        assert_eq!(live_bottom.rows.len(), viewport_rows);
        assert_eq!(
            live_bottom.row_offset + live_bottom.rows.len(),
            live_bottom.total_rows
        );
        controls.scroll_up();
        controls.scroll_up();
        let scrolled = controller_terminal_view(&ui, &runtime, &mut controls, viewport_rows)
            .expect("live view");
        assert_eq!(scrolled.scroll, 2);
        assert_eq!(
            scrolled.row_offset + scrolled.rows.len() + scrolled.scroll,
            scrolled.total_rows
        );
    }

    /// テスト用 Terminal。キー列を順に返し、描いたフレームを記録する。
    #[derive(Default)]
    struct FakeTerminal {
        keys: VecDeque<Key>,
        frames: Vec<Vec<String>>,
        waits: Vec<std::time::Duration>,
        copied: Vec<String>,
        create_call: Option<Receiver<String>>,
        observed_creates: Vec<String>,
        fail_size: bool,
        fail_draw: bool,
    }

    impl FakeTerminal {
        fn with_keys(keys: &[Key]) -> Self {
            Self {
                keys: keys.iter().cloned().collect(),
                ..Self::default()
            }
        }

        fn with_keys_waiting_for_create(keys: &[Key], create_call: Receiver<String>) -> Self {
            Self {
                create_call: Some(create_call),
                ..Self::with_keys(keys)
            }
        }
    }

    #[coverage(off)] // coverage: reason=generic_monomorphization owner=tui expires=2027-01-31 tests=production_screen_graph_terminal_harness
    impl Terminal for FakeTerminal {
        fn size(&mut self) -> io::Result<(usize, usize)> {
            if self.fail_size {
                return Err(io::Error::other("size failed"));
            }
            Ok((0, 0))
        }

        fn draw(&mut self, frame: &[String]) -> io::Result<()> {
            if self.fail_draw {
                return Err(io::Error::other("draw failed"));
            }
            self.frames.push(frame.to_vec());
            Ok(())
        }

        fn wait(&mut self, duration: std::time::Duration) -> io::Result<()> {
            self.waits.push(duration);
            Ok(())
        }

        fn read_key(&mut self) -> io::Result<Key> {
            let key = self
                .keys
                .pop_front()
                .ok_or_else(|| io::Error::other("no more keys"))?;
            // Create runs on the lifecycle worker. Tests that exercise the whole
            // terminal adapter wait at the quit boundary, making the dispatch
            // observation deterministic without changing production scheduling.
            if matches!(key, Key::CtrlQ)
                && let Some(create_call) = self.create_call.take()
            {
                let name = create_call
                    .recv_timeout(std::time::Duration::from_secs(1))
                    .map_err(|error| io::Error::other(error.to_string()))?;
                self.observed_creates.push(name);
            }
            Ok(key)
        }

        fn copy_text(&mut self, text: &str) -> Result<(), String> {
            self.copied.push(text.to_owned());
            Ok(())
        }
    }

    struct StaticMetrics;

    impl MetricsPort for StaticMetrics {
        fn latest(&mut self) -> Option<DaemonMetrics> {
            Some(DaemonMetrics {
                schema_version: 1,
                sampled_at_ms: 42,
                active_subscribers: 3,
                dropped_updates: 0,
                cpu_percent_hundredths: 250,
                resident_memory_bytes: 45 * 1024 * 1024,
                terminal_dropped_bytes: 0,
                terminal_coalesced_bytes: 0,
                terminal_backpressured_bytes: 0,
                pr_projection_dropped_bytes: 0,
                pr_projection_coalesced_bytes: 0,
                pr_projection_gaps: 0,
                agent_concurrency: None,
                failed_background_workers: 0,
            })
        }
    }

    struct StaticMetricsFactory;

    impl MetricsPortFactory for StaticMetricsFactory {
        fn create(&mut self) -> Box<dyn MetricsPort> {
            Box::new(StaticMetrics)
        }
    }

    struct IdleAgentPort;

    impl AgentCommandPort for IdleAgentPort {
        fn launch(
            &mut self,
            _operation: OperationId,
            _workspace: WorkspaceId,
            _session: Option<SessionId>,
            _profile: Option<AgentProfileId>,
        ) -> Result<AgentPaneAdmission, String> {
            Err("not launched in this test".to_owned())
        }
    }

    struct IdleAgentPortFactory;

    impl AgentCommandPortFactory for IdleAgentPortFactory {
        fn create(&mut self) -> Box<dyn AgentCommandPort> {
            Box::new(IdleAgentPort)
        }
    }

    #[test]
    fn no_metrics_factory_creates_an_empty_port() {
        assert_eq!(NoMetricsFactory.create().latest(), None);
    }

    #[test]
    fn idle_agent_port_is_safe_when_an_unexpected_launch_is_requested() {
        let mut port = IdleAgentPort;
        let error = port
            .launch(
                OperationId::new(),
                WorkspaceId::new(),
                Some(SessionId::new()),
                None,
            )
            .unwrap_err();

        assert_eq!(error, "not launched in this test");
        assert_eq!(
            port.launch_terminal(
                WorkspaceId::new(),
                Some(SessionId::new()),
                Geometry { cols: 80, rows: 24 },
                "open",
                OperationId::new(),
            )
            .unwrap_err(),
            "terminal launch is unavailable"
        );
    }

    #[derive(Default)]
    struct FakeLoader {
        opened: Vec<PathBuf>,
        cleanup_removed: Vec<PathBuf>,
        cleanup_calls: usize,
        unregistered: Vec<PathBuf>,
        unregister_calls: usize,
        created: Vec<NewRequest>,
        fail: bool,
        /// Stands in for the daemon refusing to describe the workspace being
        /// opened because it serves a different one: the loader reports it as
        /// `PermissionDenied`, which entry screens present in place.
        refuse: Option<String>,
        /// Which paths `refuse` applies to. Empty means every path, so a fence
        /// that rejects only some registered workspaces can be expressed.
        refuse_paths: Vec<PathBuf>,
        /// Number of leading `create_workspace` calls that reject before the
        /// loader starts succeeding, standing in for a pre-flight rejection
        /// (e.g. the workspace already exists) that the user then corrects.
        create_failures: usize,
        create_completions: VecDeque<WorkspaceCreateCompletion>,
        held_create: Option<WorkspaceCreateCompletion>,
        hold_create: bool,
        dispatch_error: Option<&'static str>,
        release_after_polls: Option<usize>,
        completion_noise: bool,
        opened_at: Option<DateTime<Utc>>,
    }

    impl WorkspaceLoader for FakeLoader {
        fn open(&mut self, path: &Path) -> io::Result<WorkspaceSnapshot> {
            self.opened.push(path.to_path_buf());
            let fenced = self.refuse_paths.is_empty()
                || self.refuse_paths.iter().any(|fenced| fenced == path);
            if let Some(refusal) = self.refuse.as_ref().filter(|_| fenced) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    refusal.clone(),
                ));
            }
            if self.fail {
                return Err(io::Error::other("open failed"));
            }
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("workspace");
            let mut snapshot = snapshot(name);
            if let Some(opened_at) = self.opened_at {
                snapshot.workspace.updated_at = opened_at;
            }
            Ok(snapshot)
        }

        fn cleanup_missing(&mut self, _workspaces: &[Workspace]) -> io::Result<Vec<PathBuf>> {
            self.cleanup_calls += 1;
            Ok(self.cleanup_removed.clone())
        }

        fn unregister(&mut self, paths: &[PathBuf]) -> io::Result<Vec<PathBuf>> {
            self.unregister_calls += 1;
            self.unregistered.extend_from_slice(paths);
            Ok(paths.to_vec())
        }

        fn dispatch_create(&mut self, effect: WorkspaceCreateEffect) -> io::Result<()> {
            if let Some(error) = self.dispatch_error {
                return Err(io::Error::other(error));
            }
            self.created.push(effect.request.clone());
            let completion = if self.create_failures > 0 {
                self.create_failures -= 1;
                // Mirror the real loader's pre-flight rejection: no workspace is
                // created, so the caller keeps the draft and can retry.
                WorkspaceCreateCompletion {
                    token: effect.token,
                    request: effect.request.clone(),
                    result: Err(io::Error::other(
                        "this directory is already a registered workspace",
                    )),
                }
            } else {
                // Both modes resolve to a directory that is then opened like any
                // other workspace, mirroring the real loader.
                let path = match &effect.request {
                    NewRequest::Clone { destination, .. } => destination.clone(),
                    NewRequest::Existing { path, .. } => path.clone(),
                };
                let result = self.open(&path);
                WorkspaceCreateCompletion {
                    token: effect.token,
                    request: effect.request.clone(),
                    result,
                }
            };
            if self.completion_noise {
                self.create_completions
                    .push_back(WorkspaceCreateCompletion {
                        token: WorkspaceCreateToken::new(effect.token.get() + 100),
                        request: effect.request.clone(),
                        result: Err(io::Error::other("stale completion")),
                    });
                self.create_completions
                    .push_back(WorkspaceCreateCompletion {
                        token: effect.token,
                        request: NewRequest::Existing {
                            path: PathBuf::from("wrong-request"),
                            name: "wrong-request".to_owned(),
                        },
                        result: Err(io::Error::other("mismatched completion")),
                    });
            }
            if self.hold_create {
                self.held_create = Some(completion);
            } else {
                self.create_completions.push_back(completion);
            }
            if self.completion_noise {
                self.create_completions
                    .push_back(WorkspaceCreateCompletion {
                        token: effect.token,
                        request: effect.request,
                        result: Err(io::Error::other("duplicate completion")),
                    });
            }
            Ok(())
        }

        fn take_create_completion(&mut self) -> Option<WorkspaceCreateCompletion> {
            if self.held_create.is_some()
                && let Some(remaining) = self.release_after_polls.as_mut()
            {
                if *remaining == 0 {
                    self.create_completions
                        .push_back(self.held_create.take().expect("held create"));
                } else {
                    *remaining -= 1;
                }
            }
            self.create_completions.pop_front()
        }
    }

    #[test]
    fn run_quits_from_welcome_and_handles_menu_navigation() {
        for keys in [
            vec![Key::Char('q'), Key::Enter],
            vec![Key::Quit],
            vec![Key::Escape],
            vec![Key::Down, Key::Down, Key::Up, Key::Quit],
            vec![Key::Down, Key::Down, Key::Down, Key::Enter],
        ] {
            let mut term = FakeTerminal::with_keys(&keys);
            assert_eq!(
                run(
                    &mut term,
                    Vec::new(),
                    Vec::new(),
                    now(),
                    &mut FakeLoader::default(),
                )
                .unwrap(),
                Exit::Quit
            );
            assert!(term.frames[0].join("\n").contains("Menu"));
        }
    }

    #[test]
    fn startup_splash_draws_and_paces_every_frame_without_reading_input() {
        let mut term = FakeTerminal::default();

        play_startup_splash(&mut term).unwrap();

        assert_eq!(
            term.frames.len(),
            crate::presentation::views::splash::FRAMES
        );
        assert_eq!(term.waits.len(), crate::presentation::views::splash::FRAMES);
        assert!(
            term.waits
                .iter()
                .all(|wait| *wait == crate::presentation::views::splash::ANIM_TICK)
        );
        assert!(term.keys.is_empty());
    }

    #[test]
    fn run_ignores_unknown_welcome_keys() {
        let keys = [
            Key::Char('z'),
            Key::Left,
            Key::Right,
            Key::Backspace,
            Key::Other,
            Key::Char('q'),
            Key::Enter,
        ];
        let mut term = FakeTerminal::with_keys(&keys);
        run(
            &mut term,
            Vec::new(),
            Vec::new(),
            now(),
            &mut FakeLoader::default(),
        )
        .unwrap();
        // None of these keys changes Welcome, so the gate draws the menu once
        // and every ignored key costs nothing (#554).
        assert_eq!(term.frames.len(), 1);
        assert!(
            term.frames
                .iter()
                .all(|frame| frame.join("\n").contains("Menu"))
        );
    }

    #[test]
    fn welcome_action_maps_every_destination() {
        assert!(matches!(
            welcome_action(MenuAction::Quit),
            WelcomeStep::Quit
        ));
        assert!(matches!(
            welcome_action(MenuAction::Open),
            WelcomeStep::OpenList
        ));
        assert!(matches!(
            welcome_action(MenuAction::OpenRecent(2)),
            WelcomeStep::OpenRecent(2)
        ));
        assert!(matches!(
            welcome_action(MenuAction::New),
            WelcomeStep::NewForm
        ));
        assert!(matches!(
            welcome_action(MenuAction::Config),
            WelcomeStep::ConfigScreen
        ));
    }

    #[test]
    fn config_can_be_opened_from_welcome_or_used_as_the_start() {
        let mut from_welcome =
            FakeTerminal::with_keys(&[Key::Char('c'), Key::Escape, Key::Char('q'), Key::Enter]);
        run(
            &mut from_welcome,
            Vec::new(),
            Vec::new(),
            now(),
            &mut FakeLoader::default(),
        )
        .unwrap();
        assert!(from_welcome.frames[0].join("\n").contains("Menu"));
        assert!(from_welcome.frames[1].join("\n").contains("Config"));
        assert!(from_welcome.frames[2].join("\n").contains("Menu"));

        let mut direct = FakeTerminal::with_keys(&[Key::Char('x'), Key::Quit]);
        run_from_start(
            &mut direct,
            Vec::new(),
            Vec::new(),
            now(),
            Start::Config,
            &mut FakeLoader::default(),
        )
        .unwrap();
        // `x` changes nothing on Config, so only the entry frame is drawn.
        assert_eq!(direct.frames.len(), 1);
        assert!(
            direct
                .frames
                .iter()
                .all(|frame| frame.join("\n").contains("Config"))
        );
    }

    #[test]
    fn step_config_maps_back_quit_and_stay() {
        let mut settings = DefaultSettingsPort;
        let mut config = Config::load(&mut settings);
        assert!(matches!(
            step_config(&mut config, Key::Escape, &mut settings),
            ConfigStep::Back
        ));
        assert!(matches!(
            step_config(&mut config, Key::Quit, &mut settings),
            ConfigStep::Quit
        ));
        assert!(matches!(
            step_config(&mut config, Key::Char('x'), &mut settings),
            ConfigStep::Stay
        ));
        assert!(matches!(
            step_config(&mut config, Key::Tab, &mut settings),
            ConfigStep::Stay
        ));
        for key in [
            Key::Up,
            Key::Down,
            Key::Char('j'),
            Key::Left,
            Key::Right,
            Key::CtrlQ,
        ] {
            let _ = step_config(&mut config, key, &mut settings);
        }
    }

    #[test]
    fn step_config_opens_applies_and_cancels_the_team_picker() {
        use crate::presentation::views::config::Field as ConfigField;
        use usagi_core::domain::settings::TeamTemplate;

        let mut settings = DefaultSettingsPort;
        let mut config = Config::load(&mut settings);
        for _ in 0..4 {
            step_config(&mut config, Key::Down, &mut settings);
        }
        assert_eq!(config.field(), ConfigField::TeamTemplate);
        step_config(&mut config, Key::Right, &mut settings);
        assert_eq!(config.settings().team_template, TeamTemplate::None);

        step_config(&mut config, Key::Enter, &mut settings);
        assert!(config.is_selecting_team());
        step_config(&mut config, Key::Other, &mut settings);
        step_config(&mut config, Key::Right, &mut settings);
        step_config(&mut config, Key::Right, &mut settings);
        step_config(&mut config, Key::Enter, &mut settings);
        assert!(!config.is_selecting_team());
        assert_eq!(config.settings().team_template, TeamTemplate::Flat);

        step_config(&mut config, Key::Enter, &mut settings);
        step_config(&mut config, Key::Left, &mut settings);
        step_config(&mut config, Key::Tab, &mut settings);
        step_config(&mut config, Key::Escape, &mut settings);
        assert!(!config.is_selecting_team());
        assert_eq!(config.settings().team_template, TeamTemplate::Flat);
    }

    #[test]
    fn step_config_routes_input_to_the_global_environment_editor() {
        use crate::presentation::views::config::Field as ConfigField;

        let mut settings = DefaultSettingsPort;
        let mut config = Config::load(&mut settings);
        step_config(&mut config, Key::Down, &mut settings);
        step_config(&mut config, Key::Down, &mut settings);
        assert_eq!(config.field(), ConfigField::Environment);
        step_config(&mut config, Key::Enter, &mut settings);
        assert!(config.is_editing_environment());
        step_config(&mut config, Key::Char('C'), &mut settings);
        step_config(&mut config, Key::Paste("=3xy".to_owned()), &mut settings);
        step_config(&mut config, Key::Backspace, &mut settings);
        step_config(&mut config, Key::Left, &mut settings);
        step_config(&mut config, Key::Delete, &mut settings);
        step_config(&mut config, Key::End, &mut settings);
        step_config(&mut config, Key::Enter, &mut settings);
        step_config(
            &mut config,
            Key::Paste("A=1\r\nB=2".to_owned()),
            &mut settings,
        );
        step_config(&mut config, Key::Up, &mut settings);
        step_config(&mut config, Key::Down, &mut settings);
        step_config(&mut config, Key::Home, &mut settings);
        step_config(&mut config, Key::Right, &mut settings);
        step_config(&mut config, Key::LineEnd, &mut settings);
        step_config(&mut config, Key::LineStart, &mut settings);
        step_config(&mut config, Key::Tab, &mut settings);
        assert!(!config.is_environment_save_focused());
        step_config(&mut config, Key::Other, &mut settings);
        step_config(
            &mut config,
            Key::Management {
                action: AppKey::SaveRoles,
                passthrough: vec![19],
            },
            &mut settings,
        );
        assert!(!config.is_editing_environment());
        assert_eq!(config.settings().env["A"], "1");
        assert_eq!(config.settings().env["B"], "2");
        assert_eq!(config.settings().env["C"], "3");

        step_config(&mut config, Key::Enter, &mut settings);
        assert!(config.is_editing_environment());
        step_config(&mut config, Key::Escape, &mut settings);
        assert!(!config.is_editing_environment());
    }

    #[test]
    fn step_config_saves_the_workspace_environment_from_its_save_action() {
        let mut settings = DefaultSettingsPort;
        let mut config = Config::load_workspace_with_available_models(
            &mut settings,
            AvailableAgentModels::all(),
        );
        step_config(&mut config, Key::Down, &mut settings);
        step_config(&mut config, Key::Enter, &mut settings);
        step_config(&mut config, Key::Char('A'), &mut settings);
        step_config(&mut config, Key::Paste("=1".to_owned()), &mut settings);
        step_config(&mut config, Key::Tab, &mut settings);
        assert!(config.is_environment_save_focused());
        step_config(&mut config, Key::Enter, &mut settings);

        assert!(!config.is_editing_environment());
        assert_eq!(config.settings().env["A"], "1");
    }

    #[test]
    fn step_config_saves_only_from_the_dirty_save_row() {
        let mut settings = DefaultSettingsPort;
        let mut config = Config::load(&mut settings);
        assert!(matches!(
            step_config(&mut config, Key::Enter, &mut settings),
            ConfigStep::Stay
        ));
        step_config(&mut config, Key::Right, &mut settings);
        step_config(&mut config, Key::Down, &mut settings);
        step_config(&mut config, Key::Down, &mut settings);
        step_config(&mut config, Key::Down, &mut settings);
        step_config(&mut config, Key::Down, &mut settings);
        step_config(&mut config, Key::Down, &mut settings);
        step_config(&mut config, Key::Down, &mut settings);
        step_config(&mut config, Key::Down, &mut settings);
        step_config(&mut config, Key::Down, &mut settings);
        // Enter on the dirty Save row begins the save flow (loading).
        assert!(matches!(
            step_config(&mut config, Key::Enter, &mut settings),
            ConfigStep::Save
        ));
        // A second Enter while Saving is a no-op, so it stays on the screen.
        assert!(matches!(
            step_config(&mut config, Key::Enter, &mut settings),
            ConfigStep::Stay
        ));
    }

    /// Settings port that records saves and can be told to fail, for the Config
    /// save screen-graph tests.
    #[derive(Default)]
    struct RecordingSettingsPort {
        saves: usize,
        fail_save: bool,
    }

    #[derive(Default)]
    struct WorkspaceBindingSettingsPort {
        selected: Vec<PathBuf>,
        saves: Vec<(SettingsScope, Settings)>,
    }

    impl SettingsPort for WorkspaceBindingSettingsPort {
        fn select_workspace(&mut self, workspace_root: &Path) -> io::Result<()> {
            self.selected.push(workspace_root.to_path_buf());
            Ok(())
        }

        fn read(
            &mut self,
            _scope: usagi_core::usecase::settings::SettingsScope,
        ) -> io::Result<usagi_core::domain::settings::Settings> {
            Ok(usagi_core::domain::settings::Settings {
                modal_selection_mode: usagi_core::domain::settings::ModalSelectionMode::Prompt,
                ..usagi_core::domain::settings::Settings::default()
            })
        }

        fn save(
            &mut self,
            scope: usagi_core::usecase::settings::SettingsScope,
            settings: &usagi_core::domain::settings::Settings,
        ) -> io::Result<()> {
            self.saves.push((scope, settings.clone()));
            Ok(())
        }
    }

    #[test]
    fn overview_config_saves_the_current_workspace_and_returns_to_home() {
        let mut keys = vec![Key::Char('o'), Key::Enter, Key::Char(':')];
        keys.extend("config".chars().map(Key::Char));
        keys.extend([
            Key::Enter,
            Key::Down,
            Key::Down,
            Key::Down,
            Key::Right,
            Key::Down,
            Key::Down,
            Key::Enter,
            Key::CtrlQ,
            Key::Char('y'),
        ]);
        let mut term = FakeTerminal::with_keys(&keys);
        let mut loader = FakeLoader::default();
        let mut settings = WorkspaceBindingSettingsPort::default();
        let mut sessions = UnavailableSessionCommandPortFactory;

        assert_eq!(
            run_with_settings(
                &mut term,
                vec![ws("project")],
                Vec::new(),
                now(),
                Start::Welcome,
                &mut loader,
                &mut settings,
                &mut sessions,
            )
            .unwrap(),
            Exit::Quit
        );

        assert_eq!(settings.selected, vec![PathBuf::from("/tmp/project")]);
        assert_eq!(settings.saves.len(), 1);
        assert_eq!(settings.saves[0].0, SettingsScope::Workspace);
        assert!(!settings.saves[0].1.issue_enabled);
        let frames = term
            .frames
            .iter()
            .map(|frame| frame.join("\n"))
            .collect::<Vec<_>>();
        let config = frames
            .iter()
            .position(|frame| {
                frame.contains("Config") && frame.contains("Agent") && !frame.contains("Scope:")
            })
            .expect("workspace Config is rendered");
        assert!(frames[config].contains("project"));
        assert!(!frames[config].contains("Overview"));
        let done = frames
            .iter()
            .position(|frame| frame.contains("Config") && frame.contains("[ done ]"))
            .expect("workspace Config shows done before closing");
        let returned_home = frames
            .iter()
            .skip(done + 1)
            .any(|frame| frame.contains("project") && !frame.contains("Config"));
        assert!(config < done && returned_home);
        assert_eq!(term.waits, config_save_waits(true));
    }

    #[test]
    fn screen_graph_binds_settings_for_open_recent_and_new_entries() {
        let cases = [
            (
                vec![Key::Char('o'), Key::Enter, Key::CtrlQ, Key::Char('y')],
                vec![ws("open")],
                Vec::new(),
                PathBuf::from("/tmp/open"),
            ),
            (
                vec![Key::Char('1'), Key::CtrlQ, Key::Char('y')],
                Vec::new(),
                vec![recent("recent")],
                PathBuf::from("/tmp/recent"),
            ),
            (
                vec![
                    Key::Char('e'),
                    Key::Right,
                    Key::Down,
                    Key::Char('x'),
                    Key::Enter,
                    Key::CtrlQ,
                    Key::Char('y'),
                ],
                Vec::new(),
                Vec::new(),
                PathBuf::from("/tmp/x"),
            ),
        ];

        for (keys, workspaces, recent, expected) in cases {
            let mut term = FakeTerminal::with_keys(&keys);
            let mut loader = FakeLoader::default();
            let mut settings = WorkspaceBindingSettingsPort::default();
            let mut sessions = UnavailableSessionCommandPortFactory;
            assert_eq!(
                run_with_settings(
                    &mut term,
                    workspaces,
                    recent,
                    now(),
                    Start::Welcome,
                    &mut loader,
                    &mut settings,
                    &mut sessions,
                )
                .unwrap(),
                Exit::Quit
            );
            assert_eq!(settings.selected, vec![expected]);
        }
    }

    impl SettingsPort for RecordingSettingsPort {
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
            if self.fail_save {
                return Err(io::Error::other("disk unavailable"));
            }
            self.saves += 1;
            Ok(())
        }
    }

    // Focus the dirty Save row from Global Config: cycle the theme, then step down to
    // Save (Theme → Modal mode → Environment → Agent model → Team → Issue → Memory → PR → Save).
    const CONFIG_SAVE_KEYS: [Key; 10] = [
        Key::Right,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Enter,
    ];

    // Workspace Config starts on Agent and contains Agent → env → Team →
    // Issue → Memory → Save.
    const WORKSPACE_CONFIG_SAVE_KEYS: [Key; 7] = [
        Key::Right,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Enter,
    ];

    fn config_save_waits(done: bool) -> Vec<std::time::Duration> {
        let mut waits = vec![
            crate::presentation::views::config::SAVE_WAVE_TICK;
            crate::presentation::views::config::SAVE_WAVE_FRAMES - 1
        ];
        if done {
            waits.push(crate::presentation::views::config::DONE_DISPLAY);
        }
        waits
    }

    #[test]
    fn workspace_config_handles_back_and_failed_save_without_leaving_drafts() {
        let base = vec!["home".to_owned(); 24];
        let mut settings = RecordingSettingsPort::default();
        let mut back = FakeTerminal::with_keys(&[Key::Escape]);
        run_workspace_config(&mut back, &mut settings, AvailableAgentModels::all(), &base).unwrap();

        let keys = WORKSPACE_CONFIG_SAVE_KEYS
            .iter()
            .cloned()
            .chain(std::iter::once(Key::Escape))
            .collect::<Vec<_>>();
        let mut failed = FakeTerminal::with_keys(&keys);
        let mut failing_settings = RecordingSettingsPort {
            fail_save: true,
            ..RecordingSettingsPort::default()
        };
        run_workspace_config(
            &mut failed,
            &mut failing_settings,
            AvailableAgentModels::all(),
            &base,
        )
        .unwrap();
        assert_eq!(failed.waits, config_save_waits(false));
        assert!(
            failed
                .frames
                .iter()
                .any(|frame| frame.join("\n").contains("Save failed"))
        );
    }

    #[test]
    fn workspace_config_swallows_quit_keys_until_escape() {
        let base = vec!["home".to_owned(); 24];
        let mut settings = RecordingSettingsPort::default();
        let mut term =
            FakeTerminal::with_keys(&[Key::Quit, Key::CtrlQ, Key::Char('q'), Key::Escape]);

        run_workspace_config(&mut term, &mut settings, AvailableAgentModels::all(), &base).unwrap();

        assert_eq!(term.frames.len(), 4);
        assert!(
            term.frames
                .iter()
                .all(|frame| frame.join("\n").contains("Config"))
        );
    }

    #[test]
    fn config_save_waves_then_shows_done_and_returns_home_on_its_own() {
        let keys: Vec<Key> = CONFIG_SAVE_KEYS
            .iter()
            .cloned()
            .chain(std::iter::once(Key::Quit)) // now on Welcome; quit to end the loop
            .collect();
        let mut term = FakeTerminal::with_keys(&keys);
        let mut loader = FakeLoader::default();
        let mut settings = RecordingSettingsPort::default();
        let mut sessions = UnavailableSessionCommandPortFactory;

        assert_eq!(
            run_with_settings(
                &mut term,
                Vec::new(),
                Vec::new(),
                now(),
                Start::Config,
                &mut loader,
                &mut settings,
                &mut sessions,
            )
            .unwrap(),
            Exit::Quit
        );

        // Exactly one write, one complete wave, and one confirmation dwell —
        // the screen returned home on the timer, with no extra key press.
        assert_eq!(settings.saves, 1);
        assert_eq!(term.waits, config_save_waits(true));

        // Frames appear in order: an animated Save caption, then `done`, then
        // the Welcome `Menu` reached without a key press.
        let joined: Vec<String> = term.frames.iter().map(|frame| frame.join("\n")).collect();
        let done = joined
            .iter()
            .position(|frame| frame.contains("[ done ]"))
            .expect("a done confirmation frame is drawn");
        let wave = &term.frames[done - crate::presentation::views::config::SAVE_WAVE_FRAMES..done];
        assert!(wave.iter().all(|frame| {
            frame
                .iter()
                .map(|line| strip_ansi(line))
                .collect::<Vec<_>>()
                .join("\n")
                .contains("[ Save ]")
        }));
        assert!(wave.windows(2).all(|frames| frames[0] != frames[1]));
        let menu = joined
            .iter()
            .rposition(|frame| frame.contains("Menu"))
            .expect("the Welcome menu is drawn after returning home");
        assert!(done < menu);
    }

    #[test]
    fn config_save_failure_stays_on_the_screen_without_dwelling_or_returning() {
        let keys: Vec<Key> = CONFIG_SAVE_KEYS
            .iter()
            .cloned()
            .chain([Key::Escape, Key::Quit]) // still on Config; Esc back, then quit
            .collect();
        let mut term = FakeTerminal::with_keys(&keys);
        let mut loader = FakeLoader::default();
        let mut settings = RecordingSettingsPort {
            fail_save: true,
            ..RecordingSettingsPort::default()
        };
        let mut sessions = UnavailableSessionCommandPortFactory;

        assert_eq!(
            run_with_settings(
                &mut term,
                Vec::new(),
                Vec::new(),
                now(),
                Start::Config,
                &mut loader,
                &mut settings,
                &mut sessions,
            )
            .unwrap(),
            Exit::Quit
        );

        // A failed write still animates while pending, but neither dwells on
        // `done` nor auto-returns.
        assert_eq!(settings.saves, 0);
        assert_eq!(term.waits, config_save_waits(false));

        let joined: Vec<String> = term.frames.iter().map(|frame| frame.join("\n")).collect();
        // The error is surfaced on the Config screen and no `done` confirmation
        // is ever shown.
        assert!(joined.iter().any(|frame| frame.contains("Save failed")));
        assert!(joined.iter().all(|frame| !frame.contains("[ done ]")));
    }

    #[test]
    fn new_form_opens_edits_and_returns_to_welcome() {
        let keys = [
            Key::Char('e'),
            Key::Down,
            Key::Char('a'),
            Key::Backspace,
            Key::Escape,
            Key::Char('q'),
            Key::Enter,
        ];
        let mut term = FakeTerminal::with_keys(&keys);
        run(
            &mut term,
            Vec::new(),
            Vec::new(),
            now(),
            &mut FakeLoader::default(),
        )
        .unwrap();
        assert!(term.frames[0].join("\n").contains("Menu"));
        assert!(
            term.frames[1..5]
                .iter()
                .all(|frame| frame.join("\n").contains("New Project"))
        );
        assert!(term.frames[5].join("\n").contains("Menu"));
    }

    #[test]
    fn step_new_handles_every_edit_and_exit_key() {
        let mut form = New::default();
        assert!(matches!(step_new(&mut form, Key::Down), NewStep::Stay));
        assert_eq!(form.focus(), Field::Url);
        assert!(matches!(step_new(&mut form, Key::Up), NewStep::Stay));
        assert_eq!(form.focus(), Field::Mode);
        step_new(&mut form, Key::Right);
        assert_eq!(form.mode(), Mode::Existing);
        step_new(&mut form, Key::Left);
        assert_eq!(form.mode(), Mode::Clone);
        step_new(&mut form, Key::Down);
        step_new(&mut form, Key::Char('a'));
        step_new(&mut form, Key::Char('b'));
        step_new(&mut form, Key::Left);
        step_new(&mut form, Key::Right);
        step_new(&mut form, Key::Backspace);
        for key in [
            Key::Home,
            Key::End,
            Key::LineStart,
            Key::LineEnd,
            Key::SelectLeft,
            Key::SelectRight,
            Key::SelectHome,
            Key::SelectEnd,
            Key::Delete,
            Key::Tab,
            Key::CtrlD,
            Key::Live(LiveTerminalAction::NextTab),
            Key::Click { column: 0, row: 0 },
            Key::Passthrough(Vec::new()),
        ] {
            let _ = step_new(&mut form, key);
        }
        assert_eq!(form.url(), "a");
        // Enter with a still-incomplete Clone form (no Location) validates,
        // surfaces the field error as a notice, and stays on the form.
        assert!(matches!(step_new(&mut form, Key::Enter), NewStep::Stay));
        assert_eq!(form.notice(), Some("clone location is required"));
        assert!(matches!(step_new(&mut form, Key::Other), NewStep::Stay));
        assert!(matches!(step_new(&mut form, Key::Escape), NewStep::Back));
        assert!(matches!(step_new(&mut form, Key::Quit), NewStep::Quit));
        assert!(matches!(step_new(&mut form, Key::CtrlQ), NewStep::Quit));
    }

    #[test]
    fn step_new_paste_inserts_the_pasted_text_into_the_focused_field() {
        let mut form = New::default();
        step_new(&mut form, Key::Down); // focus the Url field
        assert!(matches!(
            step_new(
                &mut form,
                Key::Paste("https://example.com/repo.git".to_owned()),
            ),
            NewStep::Stay
        ));
        assert_eq!(form.url(), "https://example.com/repo.git");
    }

    #[test]
    fn step_open_paste_appends_its_text_to_the_filter() {
        let mut open = Open::new(vec![ws("alpha")]);
        assert!(matches!(
            step_open(&mut open, Key::Paste("alp".to_owned())),
            OpenStep::Stay
        ));
        assert_eq!(open.filter(), "alp");
    }

    #[test]
    fn step_welcome_ignores_a_bracketed_paste() {
        let mut welcome = super::Welcome::new(Vec::new());
        assert!(matches!(
            super::step_welcome(&mut welcome, Key::Paste("x".to_owned())),
            super::WelcomeStep::Stay
        ));
    }

    #[test]
    fn step_new_enter_creates_once_every_required_field_is_present() {
        let mut form = New::default();
        step_new(&mut form, Key::Down); // Url
        for ch in "https://example.com/owner/repo.git".chars() {
            step_new(&mut form, Key::Char(ch));
        }
        step_new(&mut form, Key::Down); // Location
        for ch in "/projects".chars() {
            step_new(&mut form, Key::Char(ch));
        }
        // Directory は URL から導出済み。Enter で検証済みの Create を返す。
        let step = step_new(&mut form, Key::Enter);
        assert!(matches!(step, NewStep::Create(NewRequest::Clone { .. })));
    }

    #[test]
    fn new_project_notice_collapses_git_stderr_to_one_safe_line() {
        // 空メッセージは汎用の一行へフォールバックする。
        assert_eq!(
            new_project_notice(&io::Error::other(String::new())),
            "could not create the project"
        );
        // 複数行の stderr は先頭行だけを trim して残す。
        let multi = io::Error::other("fatal: repository not found\nhint: check the URL");
        assert_eq!(new_project_notice(&multi), "fatal: repository not found");
        // 長い行は省略記号付きで切り詰める。
        let long = io::Error::other("x".repeat(200));
        let notice = new_project_notice(&long);
        assert_eq!(notice.chars().count(), 72);
        assert!(notice.ends_with('…'));
    }

    #[test]
    fn safe_session_error_collapses_daemon_output_to_one_safe_line() {
        // 空メッセージは汎用の一行へフォールバックする。
        assert_eq!(safe_session_error(""), "could not create the session");
        assert_eq!(
            safe_session_error("   \n  "),
            "could not create the session"
        );
        // 複数行の出力は先頭行だけを trim して残す（後続の内部詳細を漏らさない）。
        let multi = "session name already exists\n  at daemon::lifecycle::create (secret path)";
        assert_eq!(safe_session_error(multi), "session name already exists");
        // 長い先頭行は切り詰めず全文を保つ（dialog が幅に合わせて折り返して全文表示する）。
        let notice = safe_session_error(&"x".repeat(200));
        assert_eq!(notice.chars().count(), 200);
        assert!(!notice.contains('…'));
    }

    #[test]
    fn step_new_inserts_navigation_letters_instead_of_treating_them_as_movement() {
        let mut form = New::default();
        step_new(&mut form, Key::Down); // Url
        step_new(&mut form, Key::Char('j'));
        step_new(&mut form, Key::Char('k'));
        assert_eq!(form.focus(), Field::Url);
        assert_eq!(form.url(), "jk");
    }

    #[test]
    fn quitting_from_new_exits_the_runtime() {
        let mut term = FakeTerminal::with_keys(&[Key::Char('e'), Key::Quit]);
        run(
            &mut term,
            Vec::new(),
            Vec::new(),
            now(),
            &mut FakeLoader::default(),
        )
        .unwrap();
        assert!(term.frames[1].join("\n").contains("New Project"));
    }

    #[test]
    fn new_form_enter_creates_a_workspace_and_opens_it() {
        let mut term = FakeTerminal::with_keys(&[
            Key::Char('e'), // Welcome → New
            Key::Right,     // Clone → Existing
            Key::Down,      // focus the directory path
            Key::Char('x'), // path "x"; the name derives "x"
            Key::Enter,     // valid → create and open the workspace
            Key::CtrlQ,     // leave the workspace…
            Key::Char('y'), // …confirm
        ]);
        let mut loader = FakeLoader::default();
        assert_eq!(
            run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
            Exit::Quit
        );
        // Enter dispatched exactly one create carrying the validated request.
        assert_eq!(
            loader.created,
            vec![NewRequest::Existing {
                path: PathBuf::from("x"),
                name: "x".to_owned(),
            }]
        );
        // The freshly created workspace opened on the same terminal.
        assert!(
            term.frames
                .iter()
                .any(|frame| frame.join("\n").contains("x-session"))
        );
    }

    #[test]
    fn hung_new_create_keeps_ticks_resize_escape_and_quit_responsive() {
        let mut term = FakeTerminal::with_keys(&[
            Key::Char('e'),
            Key::Right,
            Key::Down,
            Key::Char('x'),
            Key::Enter,
            Key::Other,
            Key::Resize,
            Key::Enter,
            Key::Escape,
            Key::Quit,
        ]);
        let mut loader = FakeLoader {
            hold_create: true,
            ..FakeLoader::default()
        };

        assert_eq!(
            run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
            Exit::Quit
        );
        // The second Enter is coalesced while the sole operation is pending.
        assert_eq!(loader.created.len(), 1);
        // Wake-up and resize each advance the spinner and redraw the New frame.
        let loading_frames = term
            .frames
            .iter()
            .map(|frame| frame.join("\n"))
            .filter(|frame| frame.contains("creating workspace"))
            .collect::<Vec<_>>();
        assert!(loading_frames.len() >= 3, "{loading_frames:?}");
        assert_ne!(loading_frames[0], loading_frames[1]);
        // Escape left the hung operation behind and Welcome processed Quit.
        assert!(term.frames.last().unwrap().join("\n").contains("Menu"));
    }

    #[test]
    fn loading_new_can_quit_without_waiting_for_create_completion() {
        let mut term = FakeTerminal::with_keys(&[
            Key::Char('e'),
            Key::Right,
            Key::Down,
            Key::Char('x'),
            Key::Enter,
            Key::Quit,
        ]);
        let mut loader = FakeLoader {
            hold_create: true,
            ..FakeLoader::default()
        };

        assert_eq!(
            run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
            Exit::Quit
        );
        assert_eq!(loader.created.len(), 1);
    }

    #[test]
    fn cancelled_create_completion_after_reentry_never_opens_the_workspace() {
        let mut term = FakeTerminal::with_keys(&[
            Key::Char('e'),
            Key::Right,
            Key::Down,
            Key::Char('x'),
            Key::Enter,
            Key::Escape,
            Key::Char('e'),
            Key::Quit,
        ]);
        let mut loader = FakeLoader {
            hold_create: true,
            release_after_polls: Some(2),
            ..FakeLoader::default()
        };

        assert_eq!(
            run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
            Exit::Quit
        );
        assert_eq!(loader.created.len(), 1);
        assert!(
            term.frames
                .iter()
                .all(|frame| !frame.join("\n").contains("Overview"))
        );
        let last_new = term
            .frames
            .iter()
            .rev()
            .find(|frame| frame.join("\n").contains("New Project"))
            .expect("re-entered New frame");
        assert!(last_new.join("\n").contains('x'));
    }

    #[test]
    fn reentered_new_refuses_resubmit_until_cancelled_failure_completes() {
        let mut term = FakeTerminal::with_keys(&[
            Key::Char('e'),
            Key::Right,
            Key::Down,
            Key::Char('x'),
            Key::Enter,
            Key::Escape,
            Key::Char('e'),
            Key::Enter,
            Key::Quit,
        ]);
        let mut loader = FakeLoader {
            fail: true,
            hold_create: true,
            release_after_polls: Some(3),
            ..FakeLoader::default()
        };

        assert_eq!(
            run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
            Exit::Quit
        );
        assert_eq!(loader.created.len(), 1);
        let frames = term
            .frames
            .iter()
            .map(|frame| frame.join("\n"))
            .collect::<Vec<_>>();
        assert!(
            frames
                .iter()
                .any(|frame| frame.contains("previous creation is still finishing"))
        );
        assert!(frames.iter().any(|frame| frame.contains("open failed")));
    }

    #[test]
    fn stale_and_duplicate_create_completions_open_success_exactly_once() {
        let mut term = FakeTerminal::with_keys(&[
            Key::Char('e'),
            Key::Right,
            Key::Down,
            Key::Char('x'),
            Key::Enter,
            Key::CtrlQ,
            Key::Char('y'),
        ]);
        let mut loader = FakeLoader {
            completion_noise: true,
            ..FakeLoader::default()
        };
        let mut settings = WorkspaceBindingSettingsPort::default();
        let mut factory = CountingBackendFactory::new();

        assert_eq!(
            run_screen_graph_with_backend(
                &mut term,
                Vec::new(),
                Vec::new(),
                now(),
                Start::Welcome,
                &mut loader,
                &mut settings,
                &mut factory,
                AvailableAgentModels::all(),
            )
            .unwrap(),
            Exit::Quit
        );
        assert_eq!(loader.created.len(), 1);
        assert_eq!(factory.drops_at_create.len(), 1);
    }

    /// A workspace the daemon does not serve must not be shown: its session list
    /// would be the daemon's workspace under the opened workspace's name (#549).
    /// Both switcher entries stay up with the refusal instead of tearing the TUI
    /// down, so the workspace that *is* served can be chosen next.
    #[test]
    fn a_refused_workspace_keeps_the_switcher_open_with_the_reason() {
        const REFUSAL: &str = "cannot open /tmp/recent: this daemon does not serve the selected workspace; \
             this daemon serves the workspace /tmp/served. \
             Stop it with `usagi daemon stop`, then start usagi in /tmp/recent.";

        // Welcome's Recent entry: the refusal shows on Welcome, and no workspace
        // screen is drawn for the workspace that was refused.
        let mut term = FakeTerminal::with_keys(&[Key::Char('1'), Key::Quit]);
        let mut loader = FakeLoader {
            refuse: Some(REFUSAL.to_owned()),
            ..FakeLoader::default()
        };
        assert_eq!(
            run(
                &mut term,
                Vec::new(),
                vec![recent("recent")],
                now(),
                &mut loader,
            )
            .unwrap(),
            Exit::Quit
        );
        assert_eq!(loader.opened, vec![PathBuf::from("/tmp/recent")]);
        let frames = term
            .frames
            .iter()
            .map(|frame| frame.join("\n"))
            .collect::<Vec<_>>();
        let welcome = frames
            .iter()
            .rev()
            .find(|frame| frame.contains("Menu"))
            .expect("the switcher stays on screen");
        // Wrapped over several lines, so the reason and the recovery step are both
        // present rather than clipped at the terminal width.
        assert!(contains_wrapped(welcome, REFUSAL), "{welcome}");
        assert!(!frames.iter().any(|frame| frame.contains("Overview")));

        // The Open list: same contract, presented on the list itself.
        let mut term =
            FakeTerminal::with_keys(&[Key::Char('o'), Key::Enter, Key::Escape, Key::Quit]);
        let mut loader = FakeLoader {
            refuse: Some(REFUSAL.to_owned()),
            ..FakeLoader::default()
        };
        assert_eq!(
            run(
                &mut term,
                vec![ws("served")],
                Vec::new(),
                now(),
                &mut loader,
            )
            .unwrap(),
            Exit::Quit
        );
        assert_eq!(loader.opened, vec![PathBuf::from("/tmp/served")]);
        let open_list = term
            .frames
            .iter()
            .map(|frame| frame.join("\n"))
            .rev()
            .find(|frame| frame.contains("Open Workspace"))
            .expect("the Open list stays on screen");
        assert!(contains_wrapped(&open_list, REFUSAL), "{open_list}");
    }

    /// Whether `frame` shows `text`, ignoring styling and the line breaks the
    /// notice was wrapped at.
    fn contains_wrapped(frame: &str, text: &str) -> bool {
        let squeeze = |value: &str| {
            crate::presentation::widgets::strip_ansi(value)
                .split_whitespace()
                .collect::<String>()
        };
        squeeze(frame).contains(&squeeze(text))
    }

    #[test]
    fn only_a_refusal_keeps_the_switcher_open() {
        // Every other failure still propagates: staying on the list would not
        // help, and the caller reports it.
        let mut term = FakeTerminal::with_keys(&[Key::Char('1'), Key::Quit]);
        let mut loader = FakeLoader {
            fail: true,
            ..FakeLoader::default()
        };
        let error = run(
            &mut term,
            Vec::new(),
            vec![recent("recent")],
            now(),
            &mut loader,
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "open failed");

        let mut term = FakeTerminal::with_keys(&[Key::Char('o'), Key::Enter, Key::Quit]);
        let mut loader = FakeLoader {
            fail: true,
            ..FakeLoader::default()
        };
        let error = run(
            &mut term,
            vec![ws("served")],
            Vec::new(),
            now(),
            &mut loader,
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "open failed");
    }

    #[test]
    fn new_form_enter_keeps_the_draft_and_shows_a_notice_when_creation_fails() {
        let mut term = FakeTerminal::with_keys(&[
            Key::Char('e'),
            Key::Right,
            Key::Down,
            Key::Char('x'),
            Key::Enter, // create fails
            Key::Quit,  // then quit from the still-open New form
        ]);
        let mut loader = FakeLoader {
            fail: true,
            ..FakeLoader::default()
        };
        assert_eq!(
            run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
            Exit::Quit
        );
        // The create was attempted once and the runtime stayed on the New form.
        assert_eq!(loader.created.len(), 1);
        let last_new = term
            .frames
            .iter()
            .rev()
            .find(|frame| frame.join("\n").contains("New Project"))
            .expect("still on the New screen after a failed create");
        let text = last_new.join("\n");
        assert!(text.contains("open failed")); // the failure notice
        assert!(text.contains('x')); // the draft path is retained
    }

    #[test]
    fn new_form_keeps_the_draft_when_worker_dispatch_fails() {
        let mut term = FakeTerminal::with_keys(&[
            Key::Char('e'),
            Key::Right,
            Key::Down,
            Key::Char('x'),
            Key::Enter,
            Key::Quit,
        ]);
        let mut loader = FakeLoader {
            dispatch_error: Some("worker dispatch failed"),
            ..FakeLoader::default()
        };

        assert_eq!(
            run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
            Exit::Quit
        );
        assert!(loader.created.is_empty());
        let last_new = term
            .frames
            .iter()
            .rev()
            .find(|frame| frame.join("\n").contains("New Project"))
            .expect("still on the New screen after dispatch failed");
        let text = last_new.join("\n");
        assert!(text.contains("worker dispatch failed"));
        assert!(text.contains('x'));
    }

    #[test]
    fn failed_clone_retains_every_clone_draft_field_and_mode() {
        let mut keys = vec![Key::Char('e'), Key::Down];
        keys.extend("https://example.com/acme/app.git".chars().map(Key::Char));
        keys.push(Key::Down);
        keys.extend("/tmp".chars().map(Key::Char));
        keys.push(Key::Down); // derived directory `app`
        keys.push(Key::Down);
        keys.extend("feature".chars().map(Key::Char));
        keys.extend([Key::Enter, Key::Quit]);
        let mut term = FakeTerminal::with_keys(&keys);
        let mut loader = FakeLoader {
            fail: true,
            ..FakeLoader::default()
        };

        assert_eq!(
            run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
            Exit::Quit
        );
        assert_eq!(
            loader.created,
            [NewRequest::Clone {
                repository: "https://example.com/acme/app.git".to_owned(),
                destination: PathBuf::from("/tmp/app"),
                branch: Some("feature".to_owned()),
            }]
        );
        let failed = term
            .frames
            .iter()
            .rev()
            .find(|frame| frame.join("\n").contains("open failed"))
            .expect("failed Clone frame");
        let failed = crate::presentation::widgets::strip_ansi(&failed.join("\n"));
        for value in [
            "Clone",
            "https://example.com/acme/app.git",
            "/tmp",
            "app",
            "feature",
        ] {
            assert!(failed.contains(value), "missing {value}: {failed}");
        }
    }

    #[test]
    fn new_form_recovers_after_an_existing_workspace_rejection_and_retries() {
        // The first create is rejected as if the workspace already existed; the
        // user edits the path and the second create succeeds and opens.
        let mut term = FakeTerminal::with_keys(&[
            Key::Char('e'), // Welcome → New
            Key::Right,     // Clone → Existing
            Key::Down,      // focus the directory path
            Key::Char('x'), // path "x"
            Key::Enter,     // create #1 → rejected (already registered)
            Key::Char('y'), // fix the path → "xy" (draft was retained)
            Key::Enter,     // create #2 → succeeds and opens
            Key::CtrlQ,     // leave the workspace…
            Key::Char('y'), // …confirm
        ]);
        let mut loader = FakeLoader {
            create_failures: 1,
            ..FakeLoader::default()
        };
        assert_eq!(
            run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
            Exit::Quit
        );
        // Two attempts: the rejected "x" and the corrected "xy".
        assert_eq!(
            loader.created,
            vec![
                NewRequest::Existing {
                    path: PathBuf::from("x"),
                    name: "x".to_owned(),
                },
                NewRequest::Existing {
                    path: PathBuf::from("xy"),
                    name: "xy".to_owned(),
                },
            ]
        );
        // The rejection surfaced a safe notice on the retained New form…
        assert!(
            term.frames
                .iter()
                .any(|frame| frame.join("\n").contains("already a registered workspace"))
        );
        // …and the corrected retry opened the freshly created workspace.
        assert!(
            term.frames
                .iter()
                .any(|frame| frame.join("\n").contains("xy-session"))
        );
    }

    #[test]
    fn new_form_enter_clones_and_opens_the_workspace() {
        let mut keys = vec![Key::Char('e'), Key::Down]; // New → focus Url
        keys.extend("https://example.com/o/repo.git".chars().map(Key::Char));
        keys.push(Key::Down); // focus Location
        keys.extend("/tmp".chars().map(Key::Char));
        // Directory は URL から "repo" が導出済み。
        keys.extend([Key::Enter, Key::CtrlQ, Key::Char('y')]);
        let mut term = FakeTerminal::with_keys(&keys);
        let mut loader = FakeLoader::default();
        assert_eq!(
            run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
            Exit::Quit
        );
        assert_eq!(
            loader.created,
            vec![NewRequest::Clone {
                repository: "https://example.com/o/repo.git".to_owned(),
                destination: PathBuf::from("/tmp").join("repo"),
                branch: None,
            }]
        );
        assert!(
            term.frames
                .iter()
                .any(|frame| frame.join("\n").contains("repo-session"))
        );
    }

    #[test]
    fn open_selection_loads_and_runs_workspace_on_the_same_terminal() {
        let mut term =
            FakeTerminal::with_keys(&[Key::Char('o'), Key::Enter, Key::CtrlQ, Key::Char('y')]);
        let mut loader = FakeLoader::default();
        assert_eq!(
            run(&mut term, vec![ws("alpha")], Vec::new(), now(), &mut loader,).unwrap(),
            Exit::Quit
        );
        assert_eq!(loader.opened, vec![PathBuf::from("/tmp/alpha")]);
        assert_eq!(term.frames.len(), 4);
        assert!(term.frames[0].join("\n").contains("Menu"));
        assert!(term.frames[1].join("\n").contains("Open Workspace"));
        assert!(term.frames[2].join("\n").contains("alpha-session"));
    }

    #[test]
    fn open_filter_cleanup_confirmation_and_unite_selection_use_the_injected_loader() {
        let alpha = ws("alpha");
        let beta = ws("beta");

        let mut filter = FakeTerminal::with_keys(&[Key::Char('o'), Key::Char('b'), Key::Quit]);
        run(
            &mut filter,
            vec![alpha.clone(), beta.clone()],
            Vec::new(),
            now(),
            &mut FakeLoader::default(),
        )
        .unwrap();
        assert!(filter.frames[2].join("\n").contains("↳ /tmp/beta"));

        let mut cancel =
            FakeTerminal::with_keys(&[Key::Char('o'), Key::Char('C'), Key::Char('n'), Key::Quit]);
        let mut cancel_loader = FakeLoader::default();
        run(
            &mut cancel,
            vec![alpha.clone()],
            Vec::new(),
            now(),
            &mut cancel_loader,
        )
        .unwrap();
        assert_eq!(cancel_loader.cleanup_calls, 0);

        let mut confirm =
            FakeTerminal::with_keys(&[Key::Char('o'), Key::Char('C'), Key::Char('y'), Key::Quit]);
        let mut confirm_loader = FakeLoader {
            cleanup_removed: vec![alpha.path.clone()],
            ..FakeLoader::default()
        };
        run(
            &mut confirm,
            vec![alpha.clone()],
            Vec::new(),
            now(),
            &mut confirm_loader,
        )
        .unwrap();
        assert_eq!(confirm_loader.cleanup_calls, 1);
        assert!(confirm.frames[3].join("\n").contains("No workspaces yet"));

        let mut unite = FakeTerminal::with_keys(&[
            Key::Char('o'),
            Key::Tab,
            Key::Char(' '),
            Key::Down,
            Key::Char(' '),
            Key::Enter,
            Key::Escape,
            Key::CtrlQ,
            Key::Char('y'),
        ]);
        let mut unite_loader = FakeLoader::default();
        run(
            &mut unite,
            vec![alpha, beta],
            Vec::new(),
            now(),
            &mut unite_loader,
        )
        .unwrap();
        assert_eq!(unite_loader.opened, vec![PathBuf::from("/tmp/alpha")]);
    }

    #[test]
    fn open_unregister_requires_confirmation_and_only_passes_the_selected_path_to_loader() {
        let alpha = ws("alpha");
        let beta = ws("beta");

        let mut cancel = FakeTerminal::with_keys(&[
            Key::Char('o'),
            Key::Down,
            Key::CtrlD,
            Key::Char('c'),
            Key::Quit,
        ]);
        let mut cancel_loader = FakeLoader::default();
        run(
            &mut cancel,
            vec![alpha.clone(), beta.clone()],
            Vec::new(),
            now(),
            &mut cancel_loader,
        )
        .unwrap();
        assert_eq!(cancel_loader.unregister_calls, 0);
        assert!(cancel.frames[3].join("\n").contains("Unregister workspace"));
        assert!(
            cancel.frames[3]
                .join("\n")
                .contains("Only the registry entry is removed. Files stay.")
        );
        assert!(cancel.frames[3].join("\n").contains("beta"));

        let mut confirm = FakeTerminal::with_keys(&[
            Key::Char('o'),
            Key::Down,
            Key::CtrlD,
            Key::Enter,
            Key::Quit,
        ]);
        let mut confirm_loader = FakeLoader::default();
        run(
            &mut confirm,
            vec![alpha, beta.clone()],
            Vec::new(),
            now(),
            &mut confirm_loader,
        )
        .unwrap();
        assert_eq!(confirm_loader.unregister_calls, 1);
        assert_eq!(confirm_loader.unregistered, vec![beta.path]);
        assert!(
            confirm.frames[3]
                .join("\n")
                .contains("Unregister workspace")
        );
        assert!(confirm.frames[4].join("\n").contains("alpha"));
        assert!(!confirm.frames[4].join("\n").contains("beta"));
    }

    #[test]
    fn open_navigation_keeps_workspace_open_when_escape_is_pressed() {
        // Navigate the Open list to beta and open it, confirm Escape keeps the
        // workspace open, then detach through the controller quit chord.
        let keys = [
            Key::Char('o'),
            Key::Down,
            Key::Up,
            Key::Down,
            Key::Enter,
            Key::Escape,
            Key::CtrlQ,
            Key::Char('y'),
        ];
        let mut term = FakeTerminal::with_keys(&keys);
        let mut loader = FakeLoader::default();
        run(
            &mut term,
            vec![ws("alpha"), ws("beta")],
            Vec::new(),
            now(),
            &mut loader,
        )
        .unwrap();
        assert_eq!(loader.opened, vec![PathBuf::from("/tmp/beta")]);
        assert!(
            term.frames
                .iter()
                .any(|frame| frame.join("\n").contains("beta-session"))
        );
        assert!(term.frames.iter().any(|frame| {
            frame
                .join("\n")
                .contains("No tabs stirring yet. Enter starts one.")
        }));
    }

    #[test]
    fn open_prev_wraps_and_escape_returns_to_welcome() {
        let keys = [
            Key::Char('o'),
            Key::Up,
            Key::Escape,
            Key::Char('q'),
            Key::Enter,
        ];
        let mut term = FakeTerminal::with_keys(&keys);
        run(
            &mut term,
            vec![ws("alpha"), ws("beta")],
            Vec::new(),
            now(),
            &mut FakeLoader::default(),
        )
        .unwrap();
        assert!(term.frames[1].join("\n").contains("alpha"));
        assert!(term.frames[2].join("\n").contains("beta"));
        assert!(term.frames[3].join("\n").contains("Menu"));
    }

    #[test]
    fn open_touch_keeps_workspace_open_when_escape_is_pressed() {
        let alpha = ws_minutes_ago("alpha", 20);
        let beta = ws_minutes_ago("beta", 10);
        let recent = vec![
            Recent::Workspace(WorkspaceOverview::new(beta.clone(), 2, 3, 4)),
            Recent::Workspace(WorkspaceOverview::new(alpha.clone(), 5, 6, 7)),
        ];
        let keys = [
            Key::Char('o'),
            Key::Enter,
            Key::Escape,
            Key::CtrlQ,
            Key::Char('y'),
        ];
        let mut term = FakeTerminal::with_keys(&keys);
        let mut loader = FakeLoader {
            opened_at: Some(now()),
            ..FakeLoader::default()
        };

        run(&mut term, vec![alpha, beta], recent, now(), &mut loader).unwrap();

        assert_eq!(loader.opened, vec![PathBuf::from("/tmp/alpha")]);
        assert!(
            term.frames
                .iter()
                .any(|frame| frame.join("\n").contains("alpha-session"))
        );
    }

    #[test]
    fn empty_open_enter_stays_and_open_quit_exits() {
        let keys = [Key::Char('o'), Key::Enter, Key::Down, Key::Up, Key::Quit];
        let mut term = FakeTerminal::with_keys(&keys);
        run(
            &mut term,
            Vec::new(),
            Vec::new(),
            now(),
            &mut FakeLoader::default(),
        )
        .unwrap();
        assert!(term.frames[1].join("\n").contains("No workspaces yet"));
        // Welcome and the empty Open list. Enter, Down and Up have nothing to
        // move in an empty list, so they draw nothing (#554).
        assert_eq!(term.frames.len(), 2);

        let mut term = FakeTerminal::with_keys(&[Key::Char('o'), Key::Tab, Key::Enter, Key::Quit]);
        run(
            &mut term,
            vec![ws("alpha")],
            Vec::new(),
            now(),
            &mut FakeLoader::default(),
        )
        .unwrap();
        // Welcome, the Open list, and the Home frame the chosen workspace
        // opens. Tab completes onto the only entry and changes nothing.
        assert_eq!(term.frames.len(), 3);
    }

    #[test]
    fn open_key_classifier_covers_edit_selection_and_confirmation_paths() {
        let mut open = Open::new(vec![ws("alpha"), ws("Alpha"), ws("beta")]);
        for key in [
            Key::Up,
            Key::Down,
            Key::Char('x'),
            Key::Backspace,
            Key::Left,
            Key::Right,
            Key::Home,
            Key::End,
            Key::LineStart,
            Key::LineEnd,
            Key::Delete,
            Key::SelectLeft,
            Key::SelectRight,
            Key::SelectHome,
            Key::SelectEnd,
            Key::Other,
        ] {
            assert!(matches!(step_open(&mut open, key), OpenStep::Stay));
        }
        assert!(matches!(
            step_open(&mut open, Key::Enter),
            OpenStep::Choose(_)
        ));
        assert!(matches!(step_open(&mut open, Key::Escape), OpenStep::Back));
        assert!(matches!(step_open(&mut open, Key::CtrlQ), OpenStep::Quit));

        let _ = step_open(&mut open, Key::Tab);
        let _ = step_open(&mut open, Key::Char(' '));
        let _ = step_open(&mut open, Key::Char(' '));
        let _ = step_open(&mut open, Key::Char(' '));
        assert!(matches!(
            step_open(&mut open, Key::Enter),
            OpenStep::Choose(_)
        ));

        let _ = step_open(&mut open, Key::Char('C'));
        assert!(matches!(step_open(&mut open, Key::Escape), OpenStep::Stay));
        let _ = step_open(&mut open, Key::Char('C'));
        assert!(matches!(
            step_open(&mut open, Key::Enter),
            OpenStep::ConfirmCleanup
        ));

        let _ = step_open(&mut open, Key::CtrlD);
        let _ = step_open(&mut open, Key::Left);
        assert!(matches!(step_open(&mut open, Key::Escape), OpenStep::Stay));
        let _ = step_open(&mut open, Key::CtrlD);
        assert!(matches!(
            step_open(&mut open, Key::Char('y')),
            OpenStep::ConfirmUnregister(_)
        ));

        for key in [Key::Right, Key::Tab, Key::Char('n'), Key::CtrlQ] {
            let mut open = Open::new(vec![ws("fresh")]);
            let _ = step_open(&mut open, Key::CtrlD);
            let result = step_open(&mut open, key.clone());
            assert!(matches!(result, OpenStep::Stay | OpenStep::Quit));
        }
        let mut open = Open::new(vec![ws("fresh")]);
        let _ = step_open(&mut open, Key::Char('C'));
        assert!(matches!(step_open(&mut open, Key::CtrlQ), OpenStep::Quit));
    }

    #[test]
    fn recent_loads_workspace_and_escape_keeps_it_open() {
        let mut term =
            FakeTerminal::with_keys(&[Key::Char('1'), Key::Escape, Key::CtrlQ, Key::Char('y')]);
        let mut loader = FakeLoader::default();
        run(
            &mut term,
            Vec::new(),
            vec![recent("recent")],
            now(),
            &mut loader,
        )
        .unwrap();
        assert_eq!(loader.opened, vec![PathBuf::from("/tmp/recent")]);
        assert!(term.frames[1].join("\n").contains("recent-session"));
        assert!(term.frames[2].join("\n").contains("recent-session"));
    }

    #[test]
    fn recent_touch_keeps_workspace_open_when_escape_is_pressed() {
        let alpha = ws_minutes_ago("alpha", 20);
        let beta = ws_minutes_ago("beta", 10);
        let recent = vec![
            Recent::Workspace(WorkspaceOverview::new(beta.clone(), 2, 3, 4)),
            Recent::Workspace(WorkspaceOverview::new(alpha.clone(), 5, 6, 7)),
        ];
        let keys = [Key::Char('2'), Key::Escape, Key::CtrlQ, Key::Char('y')];
        let mut term = FakeTerminal::with_keys(&keys);
        let mut loader = FakeLoader {
            opened_at: Some(now()),
            ..FakeLoader::default()
        };

        run(&mut term, vec![beta, alpha], recent, now(), &mut loader).unwrap();

        assert_eq!(loader.opened, vec![PathBuf::from("/tmp/alpha")]);
        assert!(term.frames[2].join("\n").contains("alpha-session"));
    }

    #[test]
    fn unite_recent_stays_without_loading_a_workspace() {
        let unite = Recent::Unite(UniteOverview::new(vec![
            WorkspaceOverview::new(ws("primary"), 0, 0, 0),
            WorkspaceOverview::new(ws("other"), 0, 0, 0),
        ]));
        let empty = Recent::Unite(UniteOverview::new(Vec::new()));
        let keys = [Key::Char('2'), Key::Char('1'), Key::Char('q'), Key::Enter];
        let mut term = FakeTerminal::with_keys(&keys);
        let mut loader = FakeLoader::default();
        run(
            &mut term,
            Vec::new(),
            vec![unite, empty],
            now(),
            &mut loader,
        )
        .unwrap();
        assert!(loader.opened.is_empty());
        // Both Unite selections stay on Welcome without changing it, so the
        // menu is drawn once.
        assert_eq!(term.frames.len(), 1);
    }

    #[test]
    fn missing_recent_number_stays_on_welcome() {
        let mut term = FakeTerminal::with_keys(&[Key::Char('3'), Key::Char('q'), Key::Enter]);
        run(
            &mut term,
            Vec::new(),
            vec![recent("only")],
            now(),
            &mut FakeLoader::default(),
        )
        .unwrap();
        // The out-of-range number leaves Welcome untouched, so it never redraws.
        assert_eq!(term.frames.len(), 1);
    }

    #[test]
    fn quitting_from_a_recent_workspace_exits_the_runtime() {
        let mut term = FakeTerminal::with_keys(&[Key::Char('1'), Key::CtrlQ, Key::Char('y')]);
        run(
            &mut term,
            Vec::new(),
            vec![recent("recent")],
            now(),
            &mut FakeLoader::default(),
        )
        .unwrap();
        assert_eq!(term.frames.len(), 3);
        assert!(term.frames[1].join("\n").contains("recent-session"));
    }

    #[test]
    fn workspace_loader_failure_is_propagated() {
        for (keys, recent) in [
            (vec![Key::Char('o'), Key::Enter], Vec::new()),
            (vec![Key::Char('1')], vec![recent("alpha")]),
        ] {
            let mut term = FakeTerminal::with_keys(&keys);
            let mut loader = FakeLoader {
                fail: true,
                ..FakeLoader::default()
            };
            let error = run(&mut term, vec![ws("alpha")], recent, now(), &mut loader).unwrap_err();
            assert_eq!(error.to_string(), "open failed");
        }
    }

    struct DefaultTerminalPort;
    impl AgentCommandPort for DefaultTerminalPort {
        fn launch(
            &mut self,
            _operation: OperationId,
            _workspace: WorkspaceId,
            _session: Option<SessionId>,
            _profile: Option<AgentProfileId>,
        ) -> Result<AgentPaneAdmission, String> {
            Err("agent launch is unavailable".to_owned())
        }
    }

    #[test]
    fn agent_command_port_terminal_methods_are_safe_by_default() {
        let terminal = TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: WorkspaceId::new(),
            session_id: Some(SessionId::new()),
            worktree_id: WorktreeId::new(),
        };
        let mut port = DefaultTerminalPort;
        assert!(
            port.launch(
                OperationId::new(),
                WorkspaceId::new(),
                Some(SessionId::new()),
                None
            )
            .is_err()
        );
        assert_eq!(
            port.resize_terminal(&terminal, Geometry { cols: 80, rows: 24 }),
            Ok(Geometry { cols: 80, rows: 24 })
        );
        assert_eq!(
            port.attach_terminal(&terminal, Geometry { cols: 80, rows: 24 }),
            Err(TerminalError::Unavailable)
        );
        assert_eq!(
            port.poll_terminal(&terminal, 0),
            Err(TerminalError::Unavailable)
        );
        assert_eq!(
            port.input_terminal(
                &terminal,
                TerminalSubscription { id: 1, epoch: 1 },
                0,
                OperationId::new(),
                b"x",
            ),
            Err(TerminalError::Unavailable)
        );
        // A port with no durable ledger answers unknown rather than guessing,
        // which keeps a lost acknowledgement latched instead of resent (#519).
        assert_eq!(
            port.terminal_input_outcome(&terminal, OperationId::new(), 1),
            Ok(TerminalInputResolution::Unknown)
        );
        // Detach is a no-op default and must not panic.
        port.detach_terminal(&terminal, TerminalSubscription { id: 1, epoch: 1 });
        assert_eq!(
            port.launch_terminal(
                WorkspaceId::new(),
                Some(SessionId::new()),
                Geometry { cols: 80, rows: 24 },
                "open",
                OperationId::new(),
            ),
            Err("terminal launch is unavailable".to_owned())
        );
        // The default discovers no runtimes, so an embedder without a daemon
        // simply opens a workspace with no restored panes.
        assert_eq!(port.list_terminals(), Ok(Vec::new()));
    }

    #[test]
    fn key_to_terminal_bytes_encodes_input_and_forwards_control_chords() {
        assert_eq!(key_to_terminal_bytes(Key::Char('a')), Some(b"a".to_vec()));
        assert_eq!(key_to_terminal_bytes(Key::Enter), Some(b"\r".to_vec()));
        assert_eq!(
            key_to_terminal_bytes(Key::Backspace),
            Some(b"\x7f".to_vec())
        );
        assert_eq!(key_to_terminal_bytes(Key::Tab), Some(b"\t".to_vec()));
        assert_eq!(key_to_terminal_bytes(Key::Escape), Some(b"\x1b".to_vec()));
        assert_eq!(key_to_terminal_bytes(Key::Up), Some(b"\x1b[A".to_vec()));
        assert_eq!(key_to_terminal_bytes(Key::Down), Some(b"\x1b[B".to_vec()));
        assert_eq!(key_to_terminal_bytes(Key::Right), Some(b"\x1b[C".to_vec()));
        assert_eq!(
            key_to_terminal_bytes(Key::SelectRight),
            Some(b"\x1b[C".to_vec())
        );
        assert_eq!(key_to_terminal_bytes(Key::Left), Some(b"\x1b[D".to_vec()));
        assert_eq!(
            key_to_terminal_bytes(Key::SelectLeft),
            Some(b"\x1b[D".to_vec())
        );
        for key in [Key::Home, Key::LineStart, Key::SelectHome] {
            assert_eq!(key_to_terminal_bytes(key), Some(vec![1]));
        }
        for key in [Key::End, Key::LineEnd, Key::SelectEnd] {
            assert_eq!(key_to_terminal_bytes(key), Some(vec![5]));
        }
        assert_eq!(
            key_to_terminal_bytes(Key::Delete),
            Some(b"\x1b[3~".to_vec())
        );
        assert_eq!(key_to_terminal_bytes(Key::Passthrough(Vec::new())), None);
        assert_eq!(
            key_to_terminal_bytes(Key::Passthrough(vec![0xff])),
            Some(vec![0xff])
        );
        assert_eq!(
            key_to_terminal_bytes(Key::Management {
                action: AppKey::SaveRoles,
                passthrough: vec![0x13],
            }),
            Some(vec![0x13])
        );
        // A paste is wrapped in bracketed-paste markers so the agent inserts the
        // multi-line text as one block; an empty paste sends nothing.
        assert_eq!(
            key_to_terminal_bytes(Key::Paste("a\nb".to_owned())),
            Some(b"\x1b[200~a\nb\x1b[201~".to_vec())
        );
        assert_eq!(key_to_terminal_bytes(Key::Paste(String::new())), None);
        assert_eq!(key_to_terminal_bytes(Key::Quit), Some(vec![3]));
        assert_eq!(key_to_terminal_bytes(Key::CtrlQ), Some(vec![17]));
        assert_eq!(key_to_terminal_bytes(Key::CtrlD), Some(vec![4]));
        assert_eq!(key_to_terminal_bytes(Key::Other), None);
        assert_eq!(
            key_to_terminal_bytes(Key::Live(
                crate::usecase::terminal_input::LiveTerminalAction::NextTab
            )),
            None
        );
    }

    #[test]
    fn terminal_geometry_uses_the_visible_right_pane_width() {
        assert_eq!(terminal_geometry(24, 80), Geometry { cols: 43, rows: 17 });
        // The left sidebar keeps its 36 columns; every remaining terminal
        // column belongs to the right pane even on a wide outer terminal.
        assert_eq!(
            terminal_geometry(34, 153),
            Geometry {
                cols: 116,
                rows: 27
            }
        );
        assert_eq!(
            foreground_terminal_geometry(24, 100, true),
            Geometry { cols: 56, rows: 16 }
        );
        assert_eq!(
            foreground_terminal_geometry(24, 100, false),
            terminal_geometry(24, 100)
        );
    }

    /// Welcome→Open で開いた workspace が、hard-code の `UnavailableSessionCommandPort`
    /// ではなく注入 factory から port を取り出すこと（＝本 fix）を固定する。factory が
    /// production では daemon port を返すため、これで全経路が実 port を通ることを担保する。
    #[test]
    fn open_workspace_pulls_the_session_command_port_from_the_factory() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let created = Arc::new(Mutex::new(0usize));
        let mut factory = SnapshotSessionPortFactory {
            calls: calls.clone(),
            created: created.clone(),
        };
        let keys = [Key::Char('o'), Key::Enter, Key::CtrlQ, Key::Char('y')];
        let mut term = FakeTerminal::with_keys(&keys);
        let mut loader = FakeLoader::default();
        let mut settings = DefaultSettingsPort;

        assert_eq!(
            run_with_settings(
                &mut term,
                vec![ws("alpha")],
                Vec::new(),
                now(),
                Start::Welcome,
                &mut loader,
                &mut settings,
                &mut factory,
            )
            .unwrap(),
            Exit::Quit
        );

        assert_eq!(loader.opened, vec![PathBuf::from("/tmp/alpha")]);
        assert_eq!(*created.lock().unwrap(), 1);
    }

    #[test]
    fn screen_graph_injects_metrics_when_opening_a_workspace() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut sessions = SnapshotSessionPortFactory {
            calls,
            created: Arc::new(Mutex::new(0)),
        };
        let mut agents = IdleAgentPortFactory;
        let mut metrics = StaticMetricsFactory;
        // Open the workspace, then quit it through the controller's quit chord
        // (Ctrl-Q opens the confirmation, `y` detaches); `q` alone is inert now.
        let keys = [Key::Char('o'), Key::Enter, Key::CtrlQ, Key::Char('y')];
        let mut term = FakeTerminal::with_keys(&keys);
        let mut loader = FakeLoader::default();
        let mut settings = DefaultSettingsPort;

        assert_eq!(
            run_with_settings_and_agent_and_metrics_port_factory_and_model_availability(
                &mut term,
                vec![ws("alpha")],
                Vec::new(),
                now(),
                Start::Welcome,
                &mut loader,
                &mut settings,
                &mut sessions,
                &mut agents,
                AvailableAgentModels::all(),
                &mut metrics,
            )
            .unwrap(),
            Exit::Quit
        );

        assert!(
            term.frames
                .iter()
                .flat_map(|frame| frame.iter())
                .any(|line| line.contains('\u{f2db}') && line.contains('\u{f233}'))
        );
    }

    /// Welcome の Recent 経由で開いた workspace も同じ factory から port を取り出す。
    #[test]
    fn recent_workspace_pulls_the_session_command_port_from_the_factory() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let created = Arc::new(Mutex::new(0usize));
        let mut factory = SnapshotSessionPortFactory {
            calls: calls.clone(),
            created: created.clone(),
        };
        let keys = [Key::Char('1'), Key::CtrlQ, Key::Char('y')];
        let mut term = FakeTerminal::with_keys(&keys);
        let mut loader = FakeLoader::default();
        let mut settings = DefaultSettingsPort;

        assert_eq!(
            run_with_settings(
                &mut term,
                Vec::new(),
                vec![recent("home")],
                now(),
                Start::Welcome,
                &mut loader,
                &mut settings,
                &mut factory,
            )
            .unwrap(),
            Exit::Quit
        );

        assert_eq!(loader.opened, vec![PathBuf::from("/tmp/home")]);
        assert_eq!(*created.lock().unwrap(), 1);
    }

    #[test]
    fn render_home_snapshot_draws_the_initial_home_surface() {
        // The non-interactive `usagi launch <path>` fallback renders one static
        // Home frame through the controller projection: the workspace name, its
        // sessions, and the `+ new session` row.
        let frame = render_home_snapshot(30, 100, &snapshot("demo")).join("\n");
        assert!(frame.contains("demo"));
        assert!(frame.contains("demo-session"));
        assert!(frame.contains("+ new session"));
        // A zero size safely falls back to the default geometry.
        assert!(!render_home_snapshot(0, 0, &snapshot("demo")).is_empty());

        // A Failed session in the snapshot renders with its failed treatment and
        // failure reason, so the initial fallback frame surfaces it too.
        let mut failed_snapshot = snapshot("demo");
        let id = failed_snapshot.session_ids[0];
        failed_snapshot.session_lifecycles.insert(
            id,
            usagi_core::domain::session_lifecycle::SessionLifecycleProjection {
                lifecycle: usagi_core::domain::session_lifecycle::SessionLifecycle::Failed,
                failure_stage: Some(usagi_core::domain::session_lifecycle::FailureStage::Create),
                failure_summary: Some("branch exists".into()),
            },
        );
        let failed_frame = render_home_snapshot(30, 100, &failed_snapshot).join("\n");
        assert!(failed_frame.contains("failed"));
        assert!(failed_frame.contains("branch exists"));
    }

    #[test]
    fn session_command_result_message_carries_no_projection() {
        let result = SessionCommandResult::message("daemon accepted");
        assert_eq!(result.message, "daemon accepted");
        assert!(result.sessions.is_none());
        assert!(result.session_ids.is_none());
    }

    #[test]
    fn public_value_derives_are_exercised() {
        let snapshot = snapshot("derive");
        assert_eq!(snapshot.clone(), snapshot);
        assert!(format!("{snapshot:?}").contains("derive"));
        let quit = Exit::Quit;
        assert_eq!(quit.clone(), Exit::Quit);
        assert!(format!("{quit:?}").contains("Quit"));
        let welcome = Exit::Welcome;
        assert_eq!(welcome.clone(), Exit::Welcome);
        assert_ne!(welcome, quit);
        assert!(format!("{welcome:?}").contains("Welcome"));
    }

    fn info() -> AppInfo {
        AppInfo {
            name: "usagi",
            version: "0.1.0",
        }
    }

    #[test]
    fn write_banner_writes_description_line() {
        let mut buf = Vec::new();
        write_banner(&mut buf, &info()).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "usagi v0.1.0\n");
    }

    #[test]
    fn banner_screen_runner_names_non_interactive_tui_screens() {
        let entries = [
            EntryScreen::Welcome,
            EntryScreen::Workspace {
                path: PathBuf::from("/tmp/project"),
            },
            EntryScreen::Config,
        ];
        let mut buf = Vec::new();
        let info = info();
        let mut runner = BannerScreenRunner::new(&mut buf, &info);
        for entry in &entries {
            dispatch(entry, &mut runner).unwrap();
        }
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "usagi v0.1.0: welcome TUI\n\
             usagi v0.1.0: workspace TUI (/tmp/project)\n\
             usagi v0.1.0: config TUI\n"
        );
    }

    #[test]
    fn doctor_runner_renders_checks_and_summary() {
        use crate::usecase::doctor::{CheckStatus, DiagnosticCheck, DoctorReport};

        let report = DoctorReport {
            checks: vec![
                DiagnosticCheck {
                    name: "Git",
                    status: CheckStatus::Pass,
                    detail: "git version 2.50".to_owned(),
                },
                DiagnosticCheck {
                    name: "Codex CLI",
                    status: CheckStatus::Warning,
                    detail: "not found".to_owned(),
                },
                DiagnosticCheck {
                    name: "Daemon",
                    status: CheckStatus::Fail,
                    detail: "connection refused".to_owned(),
                },
            ],
        };
        let mut buf = Vec::new();
        let info = info();
        let mut runner = BannerScreenRunner::with_doctor_report(&mut buf, &info, &report);
        dispatch(&EntryScreen::Doctor, &mut runner).unwrap();

        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "usagi v0.1.0: doctor\n\
             [ok] Git: git version 2.50\n\
             [warn] Codex CLI: not found\n\
             [error] Daemon: connection refused\n\
             result: problems found\n"
        );
    }

    #[test]
    fn doctor_runner_requires_a_report() {
        let mut buf = Vec::new();
        let info = info();
        let mut runner = BannerScreenRunner::new(&mut buf, &info);
        assert_eq!(
            dispatch(&EntryScreen::Doctor, &mut runner)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn doctor_runner_renders_a_healthy_summary() {
        use crate::usecase::doctor::DoctorReport;

        let report = DoctorReport { checks: Vec::new() };
        let mut buf = Vec::new();
        let info = info();
        let mut runner = BannerScreenRunner::with_doctor_report(&mut buf, &info, &report);
        dispatch(&EntryScreen::Doctor, &mut runner).unwrap();
        assert!(
            String::from_utf8(buf)
                .unwrap()
                .ends_with("result: healthy\n")
        );
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("write failed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn banner_screen_runner_propagates_write_failure() {
        let mut out = FailingWriter;
        out.flush().unwrap();
        let info = info();
        let mut runner = BannerScreenRunner::new(&mut out, &info);
        assert_eq!(
            dispatch(&EntryScreen::Welcome, &mut runner)
                .unwrap_err()
                .to_string(),
            "write failed"
        );
    }

    // ---- #510: interrupted Agent tabs and their explicit per-tab resume ----

    use super::{ExactAgentResume, InterruptedTab};
    use usagi_core::domain::agent::{AgentResumeRelation, AgentResumeTarget};

    /// A daemon port whose exact-target resume answers with a scripted result and
    /// counts how many requests it received.
    struct ScriptedExactResumePort {
        answers: Vec<Result<ExactAgentResume, String>>,
        requests: Arc<Mutex<Vec<(AgentResumeTarget, OperationId)>>>,
    }

    impl AgentCommandPort for ScriptedExactResumePort {
        fn launch(
            &mut self,
            _operation: OperationId,
            _workspace: WorkspaceId,
            _session: Option<SessionId>,
            _profile: Option<AgentProfileId>,
        ) -> Result<AgentPaneAdmission, String> {
            Err("agent launch is unavailable".to_owned())
        }

        fn resume_exact(
            &mut self,
            target: AgentResumeTarget,
            operation_id: OperationId,
        ) -> Result<ExactAgentResume, String> {
            self.requests.lock().unwrap().push((target, operation_id));
            if self.answers.is_empty() {
                return Err("no scripted answer".to_owned());
            }
            self.answers.remove(0)
        }
    }

    /// Wait until the scripted port has received `expected` exact-resume
    /// requests. The worker runs off-thread, so the count is polled rather than
    /// sampled after a fixed sleep.
    fn await_requests(
        requests: &Arc<Mutex<Vec<(AgentResumeTarget, OperationId)>>>,
        expected: usize,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let observed = requests.lock().unwrap().len();
            assert!(observed <= expected, "more resume requests than expected");
            if observed == expected {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the resume worker did not reach {expected} requests"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// One interrupted lineage of `session`, resumable unless `resumable` is false.
    fn interrupted_history(
        workspace: WorkspaceId,
        session: Option<SessionId>,
        resumable: bool,
    ) -> InterruptedTab {
        use usagi_core::domain::agent::{ProviderKind, ProviderResumePhase, ProviderResumeReason};
        use usagi_core::domain::id::{AgentResumeSourceId, AgentRuntimeId, WorktreeId};

        let continuation = AgentContinuationRef::new();
        let worktree_id = WorktreeId::new();
        InterruptedTab {
            continuation,
            session_id: session,
            last_terminal: TerminalRef {
                daemon_generation: DaemonGeneration::new(),
                terminal_id: TerminalId::new(),
                workspace_id: workspace,
                session_id: session,
                worktree_id,
            },
            provider: Some(ProviderKind::Claude),
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
                session_id: session,
                worktree_id,
                runtime_id: AgentRuntimeId::new(),
                adapter_revision: 3,
            }),
        }
    }

    /// The accepted answer one resume of `history` would produce.
    fn exact_resume_answer(history: &InterruptedTab) -> ExactAgentResume {
        let terminal = TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: history.last_terminal.workspace_id,
            session_id: history.session_id,
            worktree_id: history.last_terminal.worktree_id,
        };
        ExactAgentResume {
            terminal: terminal.clone(),
            continuation: Some(history.continuation),
            relation: Some(AgentResumeRelation {
                source: history.target.as_ref().unwrap().source,
                replacement_runtime: usagi_core::domain::id::AgentRuntimeId::new(),
                replacement_terminal: terminal,
            }),
        }
    }

    /// A shell driven into Closeup on `session` whose pane holds `history` as its
    /// selected interrupted tab.
    fn closeup_with_history(
        workspace: WorkspaceId,
        session: SessionId,
        history: Vec<InterruptedTab>,
        launch: Box<dyn PaneLaunchCommandPort>,
    ) -> (WorkspaceUi, WorkspaceRuntime) {
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(
                workspace,
                vec![session],
                Box::new(UnavailableAgentCommandPort),
            )
            .with_pane_launch_port(launch)
            .with_agent_tab_intent(
                workspace,
                BTreeSet::from([session]),
                Box::new(MemoryIntentPort {
                    state: Arc::new(Mutex::new(AgentTabIntent::empty(workspace))),
                    mutations: Arc::new(Mutex::new(Vec::new())),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let _ = runtime.handle_key(Key::Enter);
        let (interaction, revision) = runtime.restore_fence();
        assert!(runtime.restore_snapshot(
            interaction,
            revision,
            vec![super::PaneRestoreTarget {
                target: Target::Session(session),
                panes: Vec::new(),
                selected: None,
                selected_interrupted: None,
                interrupted: history,
            }],
        ));
        let _ = runtime.select_tab(TabDirection::Next);
        (ui, runtime)
    }

    #[test]
    fn interrupted_history_joins_its_own_scope_in_the_restore_projection() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let other = SessionId::new();
        let root_history = interrupted_history(workspace, None, true);
        let session_history = interrupted_history(workspace, Some(session), true);
        let second_session_history = interrupted_history(workspace, Some(session), false);

        let targets = super::pane_restore_targets(
            workspace,
            &BTreeSet::from([session, other]),
            AgentTabProjection::default(),
            &[],
            None,
            vec![
                root_history.clone(),
                session_history.clone(),
                second_session_history.clone(),
            ],
            &BTreeMap::from([(Some(session), second_session_history.continuation)]),
        );

        let root = targets
            .iter()
            .find(|target| target.target == Target::Root(workspace))
            .unwrap();
        assert_eq!(
            root.interrupted
                .iter()
                .map(|tab| tab.continuation)
                .collect::<Vec<_>>(),
            vec![root_history.continuation]
        );
        let managed = targets
            .iter()
            .find(|target| target.target == Target::Session(session))
            .unwrap();
        // Several histories in one scope stay separate tabs, in projection order.
        assert_eq!(
            managed.selected_interrupted,
            Some(second_session_history.continuation)
        );
        assert_eq!(
            managed
                .interrupted
                .iter()
                .map(|tab| tab.continuation)
                .collect::<Vec<_>>(),
            vec![
                session_history.continuation,
                second_session_history.continuation
            ]
        );
        // A session without history keeps an empty entry rather than borrowing
        // another scope's tabs.
        let empty = targets
            .iter()
            .find(|target| target.target == Target::Session(other))
            .unwrap();
        assert!(empty.interrupted.is_empty());
    }

    #[test]
    fn one_explicit_resume_sends_one_request_and_turns_only_that_tab_live() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let resumed = interrupted_history(workspace, Some(session), true);
        let untouched = interrupted_history(workspace, Some(session), true);
        let answer = exact_resume_answer(&resumed);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (mut ui, mut runtime) = closeup_with_history(
            workspace,
            session,
            vec![resumed.clone(), untouched.clone()],
            launch_port(Box::new(ScriptedExactResumePort {
                answers: vec![Ok(answer.clone())],
                requests: Arc::clone(&requests),
            })),
        );
        let mut pending = std::collections::HashMap::new();

        // Nothing has asked the daemon to resume anything yet.
        assert!(requests.lock().unwrap().is_empty());
        super::resume_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending);
        assert_eq!(ui.pane_launches.len(), 1);
        // A repeated activation converges to the in-flight request.
        super::resume_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending);
        assert_eq!(ui.pane_launches.len(), 1);

        super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
        std::thread::sleep(std::time::Duration::from_millis(20));
        super::drain_pane_completions_into_runtime(
            &mut ui,
            &mut runtime,
            &mut pending,
            terminal_geometry(20, 80),
        );

        // Exactly one daemon request, carrying the daemon's own opaque target.
        let requests = requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, *resumed.target.as_ref().unwrap());
        assert_eq!(runtime.focused_terminal(), Some(answer.terminal));
        // The other history tab is unchanged and still unresumed.
        assert_eq!(runtime.active_pane().tabs().len(), 2);
        assert_eq!(
            runtime
                .active_pane()
                .tabs()
                .iter()
                .filter(|tab| matches!(tab, PaneTab::Interrupted(_)))
                .count(),
            1
        );
    }

    #[test]
    fn a_refused_or_failed_resume_keeps_the_history_tab_with_safe_feedback() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let history = interrupted_history(workspace, Some(session), true);
        let mut relationless = exact_resume_answer(&history);
        relationless.relation = None;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (mut ui, mut runtime) = closeup_with_history(
            workspace,
            session,
            vec![history.clone()],
            launch_port(Box::new(ScriptedExactResumePort {
                answers: vec![
                    Err("provider resume failed; refresh Agent inventory".to_owned()),
                    Ok(relationless),
                ],
                requests: Arc::clone(&requests),
            })),
        );
        let mut pending = std::collections::HashMap::new();

        for _ in 0..2 {
            super::resume_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending);
            super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
            std::thread::sleep(std::time::Duration::from_millis(20));
            super::drain_pane_completions_into_runtime(
                &mut ui,
                &mut runtime,
                &mut pending,
                terminal_geometry(20, 80),
            );
            // The tab survives every refusal, and no live pane is invented.
            assert_eq!(runtime.active_pane().tabs().len(), 1);
            assert!(matches!(
                runtime.active_pane().tabs()[0],
                PaneTab::Interrupted(_)
            ));
            assert!(runtime.focused_terminal().is_none());
            assert!(runtime.active_pane().error().is_some());
        }
        // A transport failure and a relation-less answer are both retryable, so
        // both requests reached the daemon.
        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    #[test]
    fn an_unresumable_history_tab_never_reaches_the_daemon() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let history = interrupted_history(workspace, Some(session), false);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (mut ui, mut runtime) = closeup_with_history(
            workspace,
            session,
            vec![history],
            launch_port(Box::new(ScriptedExactResumePort {
                answers: Vec::new(),
                requests: Arc::clone(&requests),
            })),
        );
        let mut pending = std::collections::HashMap::new();

        super::resume_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending);
        super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
        assert!(ui.pane_launches.is_empty());
        assert!(requests.lock().unwrap().is_empty());
        assert_eq!(
            runtime.active_pane().error(),
            Some(
                crate::usecase::application::interrupted_tab::ResumeRejection::NotResumable
                    .safe_message()
            )
        );
    }

    #[test]
    fn closing_a_history_tab_keeps_it_visible_without_resuming_anything() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let history = interrupted_history(workspace, Some(session), true);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (mut ui, mut runtime) = closeup_with_history(
            workspace,
            session,
            vec![history.clone()],
            launch_port(Box::new(ScriptedExactResumePort {
                answers: Vec::new(),
                requests: Arc::clone(&requests),
            })),
        );
        let mut pending = std::collections::HashMap::new();
        // Seed the saved slot so the dismissal has a lineage to attach to.
        ui.mutate_agent_intent(AgentTabIntentMutation::Upsert {
            session_id: Some(session),
            continuation: history.continuation,
            terminal: history.last_terminal.clone(),
            select: false,
        })
        .unwrap();
        assert_eq!(ui.agent_slot_order(), vec![history.continuation]);

        super::close_focused_terminal_pane(&mut ui, &mut runtime, &mut pending);

        assert!(runtime.active_pane().has_tabs());
        assert!(WorkspaceUi::agent_dismissed().is_empty());
        assert_eq!(
            runtime
                .state()
                .notice()
                .map(|notice| notice.message.as_str()),
            Some("Agent tabs stay visible; exit the Agent with Ctrl-D")
        );
        assert!(requests.lock().unwrap().is_empty());
        assert!(ui.pane_launches.is_empty());
    }

    #[test]
    fn the_resume_chord_drives_the_selected_history_tab_through_the_live_surface() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let history = interrupted_history(workspace, Some(session), true);
        let answer = exact_resume_answer(&history);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (mut ui, mut runtime) = closeup_with_history(
            workspace,
            session,
            vec![history.clone()],
            launch_port(Box::new(ScriptedExactResumePort {
                answers: vec![Ok(answer.clone())],
                requests: Arc::clone(&requests),
            })),
        );
        let mut controls = LiveTerminalControls::default();
        let mut term = FakeTerminal::default();
        let mut browser = UnavailableBrowserOpener;
        let mut pending_targets = std::collections::HashMap::new();

        // `Ctrl-O r` is a pane-only control: it is consumed by the Closeup pane.
        assert!(intercept_live_terminal_control(
            &Key::Live(LiveTerminalAction::ResumeTab),
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut pending_targets,
            20,
            80,
            0,
            0,
        ));
        assert_eq!(ui.pane_launches.len(), 1);

        super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
        std::thread::sleep(std::time::Duration::from_millis(20));
        super::drain_pane_completions_into_runtime(
            &mut ui,
            &mut runtime,
            &mut pending_targets,
            terminal_geometry(20, 80),
        );
        assert_eq!(runtime.focused_terminal(), Some(answer.terminal));
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_queued_resume_waits_for_the_daemon_port_and_never_duplicates_its_request() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let first = interrupted_history(workspace, Some(session), true);
        let second = interrupted_history(workspace, Some(session), true);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (mut ui, mut runtime) = closeup_with_history(
            workspace,
            session,
            vec![first.clone(), second.clone()],
            launch_port(Box::new(ScriptedExactResumePort {
                answers: vec![
                    Ok(exact_resume_answer(&first)),
                    Ok(exact_resume_answer(&second)),
                ],
                requests: Arc::clone(&requests),
            })),
        );
        let mut pending = std::collections::HashMap::new();

        // Resume both history tabs before either answer arrives.
        super::resume_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending);
        let _ = runtime.select_tab(TabDirection::Next);
        super::resume_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending);
        assert_eq!(ui.pane_launches.len(), 2);

        // Only one worker may own the stateful daemon port: the second request
        // stays queued instead of starting a second concurrent resume.
        super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
        assert_eq!(ui.pane_launches.len(), 1);
        super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
        assert_eq!(ui.pane_launches.len(), 1);
        await_requests(&requests, 1);

        // Once the port returns with the first answer the queued one runs.
        for _ in 0..2 {
            super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
            std::thread::sleep(std::time::Duration::from_millis(20));
            super::drain_pane_completions_into_runtime(
                &mut ui,
                &mut runtime,
                &mut pending,
                terminal_geometry(20, 80),
            );
        }
        await_requests(&requests, 2);
        assert!(
            runtime
                .active_pane()
                .tabs()
                .iter()
                .all(|tab| matches!(tab, PaneTab::Live(_)))
        );
    }

    #[test]
    fn a_resume_without_an_agent_context_or_a_selected_history_tab_does_nothing() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut pending = std::collections::HashMap::new();

        // No daemon Agent context at all.
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut bare = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        super::resume_focused_interrupted_tab(&mut bare, &mut runtime, &mut pending);
        assert!(bare.pane_launches.is_empty());

        // An Agent context with no active managed target stops at the runtime
        // target boundary before looking for an interrupted tab.
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut inactive = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(workspace, Vec::new(), Box::new(UnavailableAgentCommandPort));
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
        super::resume_focused_interrupted_tab(&mut inactive, &mut runtime, &mut pending);
        assert!(inactive.pane_launches.is_empty());

        // An Agent context whose selected tab is live, not interrupted.
        let history = interrupted_history(workspace, Some(session), true);
        let (mut ui, mut runtime) = closeup_with_history(
            workspace,
            session,
            vec![history],
            launch_port(Box::new(ScriptedExactResumePort {
                answers: Vec::new(),
                requests: Arc::new(Mutex::new(Vec::new())),
            })),
        );
        let live = live_terminal_ref(workspace, session);
        let operation = OperationId::new();
        let _ = runtime.request_pane(Target::Session(session), operation, PaneKind::Agent);
        let _ = runtime.complete_pane(Target::Session(session), operation, live.clone());
        let _ = runtime.focus_terminal(Target::Session(session), live);
        super::resume_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending);
        assert!(ui.pane_launches.is_empty());
    }

    #[test]
    fn an_accepted_resume_whose_display_intent_cannot_be_saved_surfaces_a_typed_notice() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let history = interrupted_history(workspace, Some(session), true);
        let answer = exact_resume_answer(&history);
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(
                workspace,
                vec![session],
                Box::new(UnavailableAgentCommandPort),
            )
            .with_pane_launch_port(launch_port(Box::new(ScriptedExactResumePort {
                answers: vec![Ok(answer.clone())],
                requests: Arc::new(Mutex::new(Vec::new())),
            })))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::from([session]),
                Box::new(FailingIntentPort {
                    state: Arc::new(Mutex::new(AgentTabIntent::empty(workspace))),
                    error: AgentTabIntentError::Unavailable,
                    attempts: Arc::new(AtomicUsize::new(0)),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let _ = runtime.handle_key(Key::Down);
        let _ = runtime.handle_key(Key::Enter);
        let (interaction, revision) = runtime.restore_fence();
        assert!(runtime.restore_snapshot(
            interaction,
            revision,
            vec![super::PaneRestoreTarget {
                target: Target::Session(session),
                panes: Vec::new(),
                selected: None,
                selected_interrupted: None,
                interrupted: vec![history],
            }],
        ));
        let _ = runtime.select_tab(TabDirection::Next);
        let mut pending = std::collections::HashMap::new();

        super::resume_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending);
        super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
        std::thread::sleep(std::time::Duration::from_millis(20));
        super::drain_pane_completions_into_runtime(
            &mut ui,
            &mut runtime,
            &mut pending,
            terminal_geometry(20, 80),
        );

        // A daemon success is not shown as committed until display intent is
        // durable. The interrupted tab stays in place and the typed failure is
        // visible, so neither pane state nor intent bytes claims success.
        assert!(runtime.focused_terminal().is_none());
        assert!(runtime.focused_interrupted().is_some());
        assert!(runtime.state().notice().is_some());
    }

    /// One counted stand-in for every port slot of a workspace composition.
    ///
    /// It answers like the `Unavailable*` ports and counts its own drop, so
    /// "nothing this workspace established survives into the next one" is a
    /// single number rather than a per-port inspection (#556).
    struct CountedPort(Arc<AtomicUsize>);

    impl Drop for CountedPort {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl SessionCommandPort for CountedPort {}

    impl SessionRefreshPort for CountedPort {}

    impl AgentCommandPort for CountedPort {
        fn launch(
            &mut self,
            _operation: OperationId,
            _workspace: WorkspaceId,
            _session: Option<SessionId>,
            _profile: Option<AgentProfileId>,
        ) -> Result<AgentPaneAdmission, String> {
            Err("Agent launch is unavailable".to_owned())
        }
    }

    impl PaneLaunchCommandPort for CountedPort {
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
            _target: super::AgentResumeTarget,
            _operation: OperationId,
        ) -> Result<super::ExactAgentResume, String> {
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

    impl super::RestoreConnectionPort for CountedPort {
        fn take_reconnected_epoch(&mut self) -> Option<u64> {
            None
        }
    }

    impl AgentTabIntentPort for CountedPort {
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

    impl ExternalTerminalPort for CountedPort {
        fn open(&mut self, _directory: &Path) -> Result<(), String> {
            Err("external terminal launch is unavailable".to_owned())
        }
    }

    impl MetricsPort for CountedPort {
        fn latest(&mut self) -> Option<DaemonMetrics> {
            None
        }
    }

    impl BrowserOpener for CountedPort {
        fn open(&mut self, _url: &str) -> Result<(), String> {
            Err("browser opening is unavailable".to_owned())
        }
    }

    impl super::SessionWorktreeScanPort for CountedPort {
        fn scan(&mut self, _workspace: &Path) -> Vec<String> {
            Vec::new()
        }
    }

    impl BackendDecisionPort for CountedPort {
        fn refresh(&mut self, _workspace: WorkspaceId, _completions: Completions) {}

        fn resolve(
            &mut self,
            _workspace: WorkspaceId,
            _decision_id: UserDecisionId,
            _answer: UserDecisionAnswer,
            _completions: Completions,
        ) {
        }
    }

    /// Ports of one composition that the frame loop itself owns for the whole
    /// life of the workspace, and therefore drops by returning.
    ///
    /// The restore client is deliberately excluded and left uncounted: it is the
    /// one port handed to a detached worker, because quitting must never wait for
    /// a hung restore observation (#551, fixed by
    /// `blocked_restore_inventory_never_blocks_render_or_quit`). Its drop
    /// therefore happens on that worker and is not ordered against the next
    /// workspace's composition.
    const RESIDENT_PORTS_PER_COMPOSITION: usize = 11;

    /// A production-shaped factory whose every port counts its own drop, and
    /// which records how many ports had been dropped when each workspace's
    /// composition was created.
    struct CountingBackendFactory {
        drops: Arc<AtomicUsize>,
        /// `drops` observed at the start of each `create`, in entry order.
        drops_at_create: Vec<usize>,
    }

    impl CountingBackendFactory {
        fn new() -> Self {
            Self {
                drops: Arc::new(AtomicUsize::new(0)),
                drops_at_create: Vec::new(),
            }
        }

        fn port(&self) -> CountedPort {
            CountedPort(Arc::clone(&self.drops))
        }
    }

    impl super::ControllerBackendFactory for CountingBackendFactory {
        fn create(
            &mut self,
            _: &WorkspaceSnapshot,
            host: ControllerHost,
        ) -> super::ControllerBackendComposition {
            self.drops_at_create.push(self.drops.load(Ordering::SeqCst));
            super::ControllerBackendComposition {
                backend: DaemonBackend::new(
                    Box::new(host.clone()),
                    Box::new(host),
                    Box::new(UnavailableBackendPort),
                    Box::new(UnavailableBackendPort),
                )
                .with_decisions(Box::new(self.port()))
                .with_overlay(Box::new(UnavailableBackendPort)),
                session_commands: Box::new(self.port()),
                session_refresh: Box::new(self.port()),
                agent_commands: Box::new(self.port()),
                pane_launch_commands: Box::new(self.port()),
                // Uncounted on purpose: see `RESIDENT_PORTS_PER_COMPOSITION`.
                restore_commands: Box::new(UnavailableAgentCommandPort),
                session_worktrees: Box::new(self.port()),
                restore_connection: Box::new(self.port()),
                agent_tab_intents: Box::new(self.port()),
                external_terminal: Box::new(self.port()),
                metrics: Box::new(self.port()),
                browser: Box::new(self.port()),
            }
        }
    }

    /// A Recent entry with a pinned `updated_at`, so the switcher's order — and
    /// therefore which number key opens which workspace — is deterministic.
    fn recent_at(name: &str, updated_at: DateTime<Utc>) -> Recent {
        let mut workspace = ws(name);
        workspace.updated_at = updated_at;
        Recent::Workspace(WorkspaceOverview::new(workspace, 1, 0, 0))
    }

    /// #556 acceptance. Home can return to Welcome, another workspace opens from
    /// there, and none of it needs a restarted process. All three entries —
    /// Recent, Open, New — lead back to the switcher, and `Exit::Quit` stays the
    /// process-exit answer reached only by choosing `quit`.
    #[test]
    fn every_workspace_entry_returns_to_welcome_without_restarting() {
        // Recent `1` (first) → leave → Open `o`/↓/Enter (second) → leave →
        // New Existing (`x`) → leave → quit from Welcome. Each `w` is the exit
        // prompt's leave answer.
        let mut keys = vec![Key::Char('1'), Key::CtrlQ, Key::Char('w')];
        keys.extend([
            Key::Char('o'),
            Key::Down,
            Key::Enter,
            Key::CtrlQ,
            Key::Char('w'),
        ]);
        keys.extend([
            Key::Char('e'),
            Key::Right,
            Key::Down,
            Key::Char('x'),
            Key::Enter,
            Key::CtrlQ,
            Key::Char('w'),
        ]);
        // Back on Welcome for the third time, `q` ends the process.
        keys.push(Key::Char('q'));
        let mut term = FakeTerminal::with_keys(&keys);
        let mut loader = FakeLoader {
            opened_at: Some(now() + Duration::hours(1)),
            ..FakeLoader::default()
        };
        let mut settings = WorkspaceBindingSettingsPort::default();
        let mut factory = CountingBackendFactory::new();

        assert_eq!(
            run_screen_graph_with_backend(
                &mut term,
                Vec::new(),
                vec![
                    recent_at("first", now()),
                    recent_at("second", now() - Duration::hours(1)),
                ],
                now(),
                Start::Welcome,
                &mut loader,
                &mut settings,
                &mut factory,
                AvailableAgentModels::all(),
            )
            .unwrap(),
            Exit::Quit
        );

        // Three distinct workspaces opened by the one process, in entry order,
        // each one rebinding the settings port to its own root. (`loader.opened`
        // records the New form's relative path; the snapshot resolves it.)
        assert_eq!(
            loader.opened,
            vec![
                PathBuf::from("/tmp/first"),
                PathBuf::from("/tmp/second"),
                PathBuf::from("x"),
            ]
        );
        assert_eq!(
            settings.selected,
            vec![
                PathBuf::from("/tmp/first"),
                PathBuf::from("/tmp/second"),
                PathBuf::from("/tmp/x"),
            ]
        );
        assert_eq!(factory.drops_at_create.len(), 3);

        // Every departure landed on Welcome — the `Menu` heading belongs to the
        // switcher alone, so it is absent from the Open list and the New form
        // that were used to get there.
        let frames = term
            .frames
            .iter()
            .map(|frame| frame.join("\n"))
            .collect::<Vec<_>>();
        let mut from = 0;
        for workspace in ["first", "second", "x"] {
            let home = frames
                .iter()
                .skip(from)
                .position(|frame| frame.contains(workspace))
                .unwrap_or_else(|| panic!("{workspace} opens"))
                + from;
            let welcome = frames
                .iter()
                .skip(home + 1)
                .position(|frame| frame.contains("Menu"))
                .unwrap_or_else(|| panic!("leaving {workspace} draws Welcome"))
                + home
                + 1;
            assert!(
                !frames[welcome].contains("Open Workspace"),
                "leaving {workspace} must land on Welcome, not the Open list: {}",
                frames[welcome]
            );
            from = welcome;
        }
    }

    /// A workspace whose settings cannot be bound is a failure to report, not a
    /// silent entry: the error propagates out of the screen graph instead of the
    /// graph continuing with the previous workspace's settings. Every entry —
    /// Recent, Open, New — propagates it the same way.
    #[test]
    fn a_settings_binding_failure_while_opening_a_workspace_propagates() {
        struct UnbindableSettings;

        impl SettingsPort for UnbindableSettings {
            fn select_workspace(&mut self, _workspace_root: &Path) -> io::Result<()> {
                Err(io::Error::other("settings directory is unavailable"))
            }

            fn read(
                &mut self,
                _scope: usagi_core::usecase::settings::SettingsScope,
            ) -> io::Result<Settings> {
                Ok(Settings::default())
            }

            fn save(
                &mut self,
                _scope: usagi_core::usecase::settings::SettingsScope,
                _settings: &Settings,
            ) -> io::Result<()> {
                Ok(())
            }
        }

        let cases = [
            (
                vec![Key::Char('1')],
                Vec::new(),
                vec![recent_at("first", now())],
            ),
            (
                vec![Key::Char('o'), Key::Enter],
                vec![ws("listed")],
                Vec::new(),
            ),
            (
                vec![
                    Key::Char('e'),
                    Key::Right,
                    Key::Down,
                    Key::Char('x'),
                    Key::Enter,
                ],
                Vec::new(),
                Vec::new(),
            ),
        ];

        for (keys, workspaces, recent) in cases {
            let mut term = FakeTerminal::with_keys(&keys);
            let mut loader = FakeLoader::default();
            let mut factory = CountingBackendFactory::new();

            let error = run_screen_graph_with_backend(
                &mut term,
                workspaces,
                recent,
                now(),
                Start::Welcome,
                &mut loader,
                &mut UnbindableSettings,
                &mut factory,
                AvailableAgentModels::all(),
            )
            .unwrap_err();

            assert_eq!(error.to_string(), "settings directory is unavailable");
            // The failure happened before any daemon port was created, so nothing
            // was established for a workspace that never opened.
            assert!(factory.drops_at_create.is_empty());
            assert_eq!(factory.drops.load(Ordering::SeqCst), 0);
        }
    }

    /// #556 acceptance: leaving tears the workspace down. Every port of the
    /// first composition — command clients, resident lanes, restore worker
    /// connection, metrics — is dropped before the second composition exists, so
    /// no pump or subscription of the workspace that was left is still running.
    #[test]
    fn leaving_a_workspace_drops_every_port_before_the_next_one_is_created() {
        let mut term = FakeTerminal::with_keys(&[
            Key::Char('1'),
            Key::CtrlQ,
            Key::Char('w'),
            Key::Char('2'),
            Key::CtrlQ,
            Key::Char('q'),
        ]);
        let mut loader = FakeLoader {
            opened_at: Some(now() + Duration::hours(1)),
            ..FakeLoader::default()
        };
        let mut settings = WorkspaceBindingSettingsPort::default();
        let mut factory = CountingBackendFactory::new();

        run_screen_graph_with_backend(
            &mut term,
            Vec::new(),
            vec![
                recent_at("first", now()),
                recent_at("second", now() - Duration::hours(1)),
            ],
            now(),
            Start::Welcome,
            &mut loader,
            &mut settings,
            &mut factory,
            AvailableAgentModels::all(),
        )
        .unwrap();

        // Exactly two compositions, and the second one started with the first
        // one already fully torn down: residue would show as a shortfall here.
        assert_eq!(
            factory.drops_at_create,
            vec![0, RESIDENT_PORTS_PER_COMPOSITION]
        );
        // After the run, the second composition is gone too: nothing outlives it.
        assert_eq!(
            factory.drops.load(Ordering::SeqCst),
            2 * RESIDENT_PORTS_PER_COMPOSITION
        );
    }

    /// #556 acceptance: the workspace fence still refuses, and it refuses as a
    /// notice on the screen the user is standing on — including the Welcome that
    /// was reached by leaving a workspace. No silent fallback to the workspace
    /// that was left.
    #[test]
    fn a_fenced_workspace_refuses_on_the_welcome_reached_by_leaving() {
        const REFUSAL: &str = "cannot open /tmp/second: this daemon does not serve the selected \
             workspace; this daemon serves the workspace /tmp/first.";

        let mut term = FakeTerminal::with_keys(&[
            Key::Char('1'),
            Key::CtrlQ,
            Key::Char('w'),
            // The fenced workspace keeps Welcome up; the served one still opens.
            Key::Char('2'),
            Key::Char('1'),
            Key::CtrlQ,
            Key::Char('q'),
        ]);
        let mut loader = FakeLoader {
            refuse: Some(REFUSAL.to_owned()),
            refuse_paths: vec![PathBuf::from("/tmp/second")],
            opened_at: Some(now() + Duration::hours(1)),
            ..FakeLoader::default()
        };
        let mut settings = WorkspaceBindingSettingsPort::default();
        let mut factory = CountingBackendFactory::new();

        assert_eq!(
            run_screen_graph_with_backend(
                &mut term,
                Vec::new(),
                vec![
                    recent_at("first", now()),
                    recent_at("second", now() - Duration::hours(1)),
                ],
                now(),
                Start::Welcome,
                &mut loader,
                &mut settings,
                &mut factory,
                AvailableAgentModels::all(),
            )
            .unwrap(),
            Exit::Quit
        );

        // The refused open was attempted and reported; only the served workspace
        // ever became a composition.
        assert_eq!(
            loader.opened,
            vec![
                PathBuf::from("/tmp/first"),
                PathBuf::from("/tmp/second"),
                PathBuf::from("/tmp/first"),
            ]
        );
        assert_eq!(factory.drops_at_create.len(), 2);
        let welcome = term
            .frames
            .iter()
            .map(|frame| frame.join("\n"))
            .rev()
            .find(|frame| frame.contains("Menu"))
            .expect("the switcher stays on screen after the refusal");
        assert!(contains_wrapped(&welcome, REFUSAL), "{welcome}");
    }

    /// A workspace opened directly (`usagi <path>`) has no Welcome behind it, so
    /// the runner reports the choice and the composition root decides. Quitting
    /// and leaving must be different answers here too.
    #[test]
    fn a_direct_workspace_reports_leaving_and_quitting_as_different_exits() {
        for (key, expected) in [
            (Key::Char('w'), Exit::Welcome),
            (Key::Char('q'), Exit::Quit),
        ] {
            let mut term = FakeTerminal::with_keys(&[Key::CtrlQ, key.clone()]);
            let mut factory = FixedBackendFactory {
                sessions: Some(Box::new(UnavailableSessionCommandPort)),
                agent: Some(Box::new(UnavailableAgentCommandPort)),
                launch: None,
                restore: None,
                metrics: Some(Box::new(NoMetrics)),
                browser: Some(Box::new(UnavailableBrowserOpener)),
                session_refresh: None,
                decisions: None,
                session_worktrees: None,
            };

            assert_eq!(
                run_workspace_controller_with_backend(&mut term, snapshot("direct"), &mut factory,)
                    .unwrap(),
                expected,
                "{key:?}"
            );
        }
    }

    fn has_director_drawer(frames: &[Vec<String>]) -> bool {
        frames.iter().any(|frame| {
            let text = frame.join("\n");
            text.contains("♛ Director")
                && (text.contains("No conversations yet") || text.contains("Organization"))
                && text.contains("[ New ]")
        })
    }

    #[test]
    fn direct_welcome_recent_and_open_entries_share_the_director_drawer_shell() {
        let mut direct = FakeTerminal::with_keys(&[
            Key::Live(LiveTerminalAction::Director),
            Key::Escape,
            Key::CtrlQ,
            Key::Char('q'),
        ]);
        let mut direct_factory = FixedBackendFactory {
            sessions: Some(Box::new(UnavailableSessionCommandPort)),
            agent: Some(Box::new(UnavailableAgentCommandPort)),
            launch: None,
            restore: None,
            metrics: Some(Box::new(NoMetrics)),
            browser: Some(Box::new(UnavailableBrowserOpener)),
            session_refresh: None,
            decisions: None,
            session_worktrees: None,
        };
        assert_eq!(
            run_workspace_controller_with_backend(
                &mut direct,
                snapshot("direct"),
                &mut direct_factory,
            )
            .unwrap(),
            Exit::Quit
        );
        assert!(has_director_drawer(&direct.frames));

        let mut recent_term = FakeTerminal::with_keys(&[
            Key::Char('1'),
            Key::Live(LiveTerminalAction::Director),
            Key::Escape,
            Key::CtrlQ,
            Key::Char('q'),
        ]);
        run(
            &mut recent_term,
            Vec::new(),
            vec![recent("recent")],
            now(),
            &mut FakeLoader::default(),
        )
        .unwrap();
        assert!(has_director_drawer(&recent_term.frames));

        let mut open_term = FakeTerminal::with_keys(&[
            Key::Char('o'),
            Key::Enter,
            Key::Live(LiveTerminalAction::Director),
            Key::Escape,
            Key::CtrlQ,
            Key::Char('q'),
        ]);
        run(
            &mut open_term,
            vec![ws("open")],
            Vec::new(),
            now(),
            &mut FakeLoader::default(),
        )
        .unwrap();
        assert!(has_director_drawer(&open_term.frames));
    }

    /// A terminal that only implements the required port methods keeps the old
    /// pacing: `wait_for_key` waits the frame out and reports no input.
    #[derive(Default)]
    struct SleepingTerminal {
        frames: usize,
        waits: Vec<std::time::Duration>,
    }

    impl Terminal for SleepingTerminal {
        fn size(&mut self) -> io::Result<(usize, usize)> {
            Ok((24, 80))
        }

        fn draw(&mut self, _frame: &[String]) -> io::Result<()> {
            self.frames += 1;
            Ok(())
        }

        fn wait(&mut self, duration: std::time::Duration) -> io::Result<()> {
            self.waits.push(duration);
            Ok(())
        }

        fn read_key(&mut self) -> io::Result<Key> {
            Ok(Key::Quit)
        }
    }

    /// Feeds one key at a chosen splash frame; every other frame reports the
    /// keys given for it, then no input.
    struct SplashTerminal {
        frames: Vec<Vec<String>>,
        answers: VecDeque<Option<Key>>,
        keys: VecDeque<Key>,
    }

    impl SplashTerminal {
        fn new(answers: Vec<Option<Key>>) -> Self {
            Self {
                frames: Vec::new(),
                answers: answers.into(),
                keys: VecDeque::new(),
            }
        }

        fn with_keys(mut self, keys: &[Key]) -> Self {
            self.keys = keys.iter().cloned().collect();
            self
        }
    }

    impl Terminal for SplashTerminal {
        fn size(&mut self) -> io::Result<(usize, usize)> {
            Ok((24, 80))
        }

        fn draw(&mut self, frame: &[String]) -> io::Result<()> {
            self.frames.push(frame.to_vec());
            Ok(())
        }

        fn wait(&mut self, _duration: std::time::Duration) -> io::Result<()> {
            panic!("the splash waits on the input-aware path, not on a bare sleep");
        }

        fn read_key(&mut self) -> io::Result<Key> {
            self.keys
                .pop_front()
                .ok_or_else(|| io::Error::other("no more keys"))
        }

        fn wait_for_key(&mut self, _duration: std::time::Duration) -> io::Result<Option<Key>> {
            Ok(self.answers.pop_front().flatten())
        }
    }

    /// #556 acceptance: the splash is skippable. A key press during it ends the
    /// animation at that frame instead of holding the terminal for its full
    /// 14-frame run.
    #[test]
    fn a_key_press_skips_the_rest_of_the_startup_splash() {
        let frames = crate::presentation::views::splash::FRAMES;
        // No input at all: every frame plays, as before.
        let mut full = SplashTerminal::new(vec![None; frames]);
        assert_eq!(play_startup_splash(&mut full).unwrap(), frames);
        assert_eq!(full.frames.len(), frames);

        // A key on the third frame ends it there.
        let mut skipped = SplashTerminal::new(vec![None, None, Some(Key::Char('o'))]);
        assert_eq!(play_startup_splash(&mut skipped).unwrap(), 3);
        assert_eq!(skipped.frames.len(), 3);

        // A wake-up tick and a resize are not key presses: they keep the pace,
        // and the next frame re-reads the terminal size.
        let mut paced = SplashTerminal::new(
            std::iter::repeat_n(Some(Key::Other), frames / 2)
                .chain(std::iter::repeat_n(Some(Key::Resize), frames - frames / 2))
                .collect(),
        );
        assert_eq!(play_startup_splash(&mut paced).unwrap(), frames);
        assert_eq!(paced.frames.len(), frames);
    }

    #[test]
    fn a_terminal_without_an_input_aware_wait_keeps_the_old_splash_pacing() {
        let mut term = SleepingTerminal::default();

        let played = play_startup_splash(&mut term).unwrap();

        let frames = crate::presentation::views::splash::FRAMES;
        assert_eq!(played, frames);
        assert_eq!(term.frames, frames);
        assert_eq!(term.waits.len(), frames);
        assert!(
            term.waits
                .iter()
                .all(|wait| *wait == crate::presentation::views::splash::ANIM_TICK)
        );
    }

    /// #556 acceptance: the splash belongs to launching the process, not to
    /// arriving at Welcome. The Welcome reached by leaving a workspace draws no
    /// splash frame at all.
    #[test]
    fn the_startup_splash_plays_once_per_process() {
        let mut splash = super::StartupSplash::new();
        let frames = crate::presentation::views::splash::FRAMES;

        let mut term = SplashTerminal::new(vec![None; frames]);
        assert_eq!(splash.play(&mut term).unwrap(), frames);
        assert_eq!(term.frames.len(), frames);

        // Returning to Welcome is not a launch: no size read, no draw, no wait.
        let mut again = SplashTerminal::new(Vec::new());
        assert_eq!(splash.play(&mut again).unwrap(), 0);
        assert!(again.frames.is_empty());
    }

    /// #556 acceptance: an interrupted splash still hands Welcome its correct
    /// initial state. The skip key is consumed by the skip, so Welcome starts on
    /// its own first frame and reads its own first input.
    #[test]
    fn welcome_starts_correctly_after_an_interrupted_splash() {
        let mut term = SplashTerminal::new(vec![Some(Key::Char('o'))]).with_keys(&[
            Key::Char('1'),
            Key::CtrlQ,
            Key::Char('q'),
        ]);
        let mut splash = super::StartupSplash::new();
        let played = splash.play(&mut term).unwrap();
        assert_eq!(played, 1);

        let mut loader = FakeLoader::default();
        assert_eq!(
            run_from_start(
                &mut term,
                Vec::new(),
                vec![recent_at("first", now())],
                now(),
                Start::Welcome,
                &mut loader,
            )
            .unwrap(),
            Exit::Quit
        );

        // The frame right after the skipped splash is the switcher, drawn from
        // the given Recent; Welcome's own keys then drive it as usual.
        let welcome = term.frames[played].join("\n");
        assert!(welcome.contains("Menu"), "{welcome}");
        assert!(welcome.contains("first"), "{welcome}");
        assert_eq!(loader.opened, vec![PathBuf::from("/tmp/first")]);
    }
}

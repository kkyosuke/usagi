//! TUI 面へ実端末と filesystem を接続する composition adapter。

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use std::{sync::mpsc, thread};

use chrono::Utc;
use crossterm::cursor;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::terminal::{
    self, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{execute, queue};
use usagi_core::domain::AppInfo;
use usagi_core::domain::id::WorkspaceId;
use usagi_core::domain::note::Scratchpad;
use usagi_core::domain::recent::Recent;
use usagi_core::domain::session::{SessionOrigin, SessionRecord};
use usagi_core::domain::session_lifecycle::ManagedSession;
use usagi_core::domain::settings::Settings;
use usagi_core::domain::terminal_launch::{
    TerminalLaunchRequest, TerminalLaunchScope, TerminalProfileId,
};
use usagi_core::domain::workspace::Workspace;
use usagi_core::infrastructure::error_log::ErrorLog;
use usagi_core::infrastructure::git::diff_status;
use usagi_core::infrastructure::store::state::WorkspaceStateStore;
use usagi_core::infrastructure::store::workspace::Storage;
use usagi_core::usecase::client::{
    AgentLaunchIntent, ClientError, ClientPolicy, DaemonClient, DaemonMetrics, DaemonReply,
    DaemonRequest, IpcClient, MetricsAction, SessionAction, TerminalAction, TerminalGeometry,
    TerminalLaunchIntent, TerminalRequest,
};
use usagi_core::usecase::settings::{SettingsPort, SettingsScope};
use usagi_core::usecase::workspace as workspace_usecase;
use usagi_daemon::usecase::session_runtime::SystemGit;
use usagi_tui::infrastructure::metrics::MetricsHook;
use usagi_tui::presentation::frame::{Frame, FrameRenderer};
use usagi_tui::presentation::views::config::{self, AvailableAgentModels, Config};
use usagi_tui::presentation::views::welcome::{self, Welcome};
use usagi_tui::presentation::views::workspace::{self, GitDiff, Workspace as WorkspaceView};
use usagi_tui::presentation::{
    self, AgentCommandPort, AgentCommandPortFactory, BannerScreenRunner, Exit, MetricsPort,
    MetricsPortFactory, SessionCommandPort, SessionCommandPortFactory, SessionCommandResult, Start,
    WorkspaceLoader, WorkspaceSnapshot,
};
use usagi_tui::usecase::application::pane_runtime::Geometry;
use usagi_tui::usecase::application::terminal_session::{
    TerminalAttach, TerminalChunk, TerminalError,
};
use usagi_tui::usecase::application::{self, EntryScreen, Key, Terminal};
use usagi_tui::usecase::overview::SessionCommand;
use usagi_tui::usecase::terminal_input::{
    KeyCode, KeyEventKind, LiveInput, LiveInputClassifier, LiveInputOutput, Modifiers, RuntimeEvent,
};

use crate::runtime::clipboard::PlatformClipboard;
use crate::tui_input::{CrosstermSource, EventPump, NoBackend};

/// Composition adapter for Overview's daemon-owned session lifecycle commands.
#[derive(Default)]
struct DaemonSessionCommandPort {
    last_revision: u64,
}

struct DaemonMetricsPort {
    last_sample: Option<Instant>,
    latest: Option<DaemonMetrics>,
    git_diffs: BTreeMap<usagi_core::domain::id::SessionId, GitDiff>,
    git_receiver: Option<mpsc::Receiver<(usagi_core::domain::id::SessionId, GitDiff)>>,
    last_git_refresh: Option<Instant>,
}
impl DaemonMetricsPort {
    // Composition-only adapter: it constructs the real daemon client and uses
    // the monotonic clock. The presentation `MetricsPort` is covered with fakes.
    #[coverage(off)]
    const fn new() -> Self {
        Self {
            last_sample: None,
            latest: None,
            git_diffs: BTreeMap::new(),
            git_receiver: None,
            last_git_refresh: None,
        }
    }
}
impl MetricsPort for DaemonMetricsPort {
    // Real daemon I/O belongs to the composition root; UI behaviour is tested
    // through its injected MetricsPort boundary.
    #[coverage(off)]
    fn latest(&mut self) -> Option<DaemonMetrics> {
        if self
            .last_sample
            .is_some_and(|sample| sample.elapsed() < Duration::from_secs(1))
        {
            return self.latest.clone();
        }
        self.last_sample = Some(Instant::now());
        let mut client = crate::runtime::daemon::client(ClientPolicy::tui()).ok()?;
        match client
            .request(DaemonRequest::Metrics {
                action: MetricsAction::Snapshot,
            })
            .ok()?
        {
            DaemonReply::Ok(value) => {
                self.latest = serde_json::from_value(value).ok();
                self.latest.clone()
            }
            DaemonReply::Accepted { .. } => None,
        }
    }

    #[coverage(off)]
    fn git_diffs(
        &mut self,
        sessions: &[(usagi_core::domain::id::SessionId, PathBuf)],
    ) -> BTreeMap<usagi_core::domain::id::SessionId, GitDiff> {
        let active_ids = sessions.iter().map(|(id, _)| *id).collect::<Vec<_>>();
        self.git_diffs.retain(|id, _| active_ids.contains(id));
        let mut finished = false;
        if let Some(receiver) = &self.git_receiver {
            loop {
                match receiver.try_recv() {
                    Ok((id, status)) => {
                        self.git_diffs.insert(id, status);
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        finished = true;
                        break;
                    }
                }
            }
        }
        if finished {
            self.git_receiver = None;
        }
        if self.git_receiver.is_none()
            && self
                .last_git_refresh
                .is_none_or(|last| last.elapsed() >= Duration::from_secs(1))
        {
            let (sender, receiver) = mpsc::channel();
            let sessions = sessions.to_vec();
            thread::spawn(move || {
                let runner = SystemGit;
                for (id, path) in sessions {
                    let Ok(Some(status)) = diff_status(&runner, &path) else {
                        continue;
                    };
                    let _ = sender.send((
                        id,
                        GitDiff {
                            base: status.base,
                            ahead: status.ahead,
                            behind: status.behind,
                            added: status.added,
                            removed: status.removed,
                        },
                    ));
                }
            });
            self.git_receiver = Some(receiver);
            self.last_git_refresh = Some(Instant::now());
        }
        self.git_diffs.clone()
    }
}

struct DaemonMetricsPortFactory;

impl MetricsPortFactory for DaemonMetricsPortFactory {
    #[coverage(off)]
    fn create(&mut self) -> Box<dyn MetricsPort> {
        Box::new(DaemonMetricsPort::new())
    }
}

/// Root composition adapter for the only Agent launch authority: the daemon.
///
/// Terminal streaming keeps one persistent daemon connection for its lifetime:
/// the daemon fences a terminal subscription (and therefore input/detach) to the
/// connection that attached it, so attach, poll and input must share it.
#[derive(Default)]
struct DaemonAgentCommandPort {
    terminal: Option<IpcClient<std::os::unix::net::UnixStream>>,
}

impl DaemonAgentCommandPort {
    #[coverage(off)]
    const fn new() -> Self {
        Self { terminal: None }
    }

    /// Returns the persistent terminal connection, opening it on first use.
    #[coverage(off)]
    fn terminal_client(
        &mut self,
    ) -> Result<&mut IpcClient<std::os::unix::net::UnixStream>, TerminalError> {
        if self.terminal.is_none() {
            self.terminal = Some(
                crate::runtime::daemon::client(ClientPolicy::tui())
                    .map_err(|_| TerminalError::Unavailable)?,
            );
        }
        Ok(self
            .terminal
            .as_mut()
            .expect("terminal client was just set"))
    }

    /// Sends one terminal request over the persistent connection and returns its
    /// success body.  A transport failure drops the connection so the next
    /// attach reconnects instead of reusing a broken socket.
    #[coverage(off)]
    fn terminal_request(
        &mut self,
        action: TerminalAction,
        request: TerminalRequest,
    ) -> Result<serde_json::Value, TerminalError> {
        let payload = serde_json::to_value(request).expect("terminal request is serializable");
        let reply = {
            let client = self.terminal_client()?;
            client.request(DaemonRequest::Terminal { action, payload })
        };
        match reply {
            Ok(DaemonReply::Ok(body) | DaemonReply::Accepted { body, .. }) => Ok(body),
            Err(error) => {
                self.terminal = None;
                Err(map_terminal_error(&error))
            }
        }
    }
}

struct DaemonAgentCommandPortFactory;

impl AgentCommandPortFactory for DaemonAgentCommandPortFactory {
    #[coverage(off)]
    fn create(&mut self) -> Box<dyn AgentCommandPort> {
        Box::new(DaemonAgentCommandPort::new())
    }
}

/// Maps a typed client failure onto the safe terminal feedback the UI renders.
/// No mapping authorizes a local PTY fallback.
#[coverage(off)]
fn map_terminal_error(error: &usagi_core::usecase::client::ClientError) -> TerminalError {
    use usagi_core::infrastructure::ipc::ErrorCode;
    match error.code() {
        ErrorCode::ResyncRequired => TerminalError::ResyncRequired,
        ErrorCode::StaleTarget => TerminalError::Stale,
        ErrorCode::OwnershipUnknown => TerminalError::Orphaned,
        _ => TerminalError::Unavailable,
    }
}

impl AgentCommandPort for DaemonAgentCommandPort {
    #[coverage(off)]
    fn launch(
        &mut self,
        workspace: WorkspaceId,
        session: usagi_core::domain::id::SessionId,
        profile: Option<usagi_core::domain::agent::AgentProfileId>,
    ) -> Result<usagi_core::domain::id::TerminalRef, String> {
        let mut client =
            crate::runtime::daemon::client(usagi_core::usecase::client::ClientPolicy::tui())
                .map_err(|_| "daemon unavailable; reconnect to continue".to_owned())?;
        let operation_id = usagi_core::domain::id::OperationId::new().to_string();
        match client
            .request(DaemonRequest::Agent {
                operation_id,
                intent: AgentLaunchIntent {
                    workspace,
                    session,
                    profile,
                },
            })
            .map_err(|_| "daemon request failed; reconnect to continue".to_owned())?
        {
            DaemonReply::Accepted { body, .. } | DaemonReply::Ok(body) => body
                .get("terminal")
                .cloned()
                .ok_or_else(|| "agent launch was not accepted".to_owned())
                .and_then(|terminal| {
                    serde_json::from_value(terminal)
                        .map_err(|_| "agent launch returned an invalid terminal".to_owned())
                }),
        }
    }

    #[coverage(off)]
    fn launch_terminal(
        &mut self,
        workspace: WorkspaceId,
        session: usagi_core::domain::id::SessionId,
        geometry: usagi_tui::usecase::application::pane_runtime::Geometry,
    ) -> Result<usagi_core::domain::id::TerminalRef, String> {
        let lifecycle = request_lifecycle_snapshot()
            .map_err(|_| "daemon unavailable; reconnect to continue".to_owned())?;
        let managed = lifecycle
            .sessions
            .iter()
            .find(|candidate| {
                lifecycle.workspace_id == workspace && candidate.session_id == session
            })
            .ok_or_else(|| "selected session is no longer available".to_owned())?;
        if managed.lifecycle != usagi_core::domain::session_lifecycle::SessionLifecycle::Available {
            return Err("selected session is not ready for a terminal".to_owned());
        }
        let intent = TerminalLaunchIntent {
            request: TerminalLaunchRequest {
                profile_id: TerminalProfileId::new("login-shell")
                    .expect("static terminal profile is valid"),
                scope: TerminalLaunchScope {
                    workspace_id: workspace,
                    session_id: Some(session),
                    worktree_id: managed.worktree_id,
                },
            },
            geometry: TerminalGeometry {
                cols: geometry.cols,
                rows: geometry.rows,
            },
        };
        let mut client = crate::runtime::daemon::client(ClientPolicy::tui())
            .map_err(|_| "daemon unavailable; reconnect to continue".to_owned())?;
        let payload = serde_json::to_value(TerminalRequest::Launch { intent })
            .expect("terminal request is serializable");
        match client
            .request(DaemonRequest::Terminal {
                action: TerminalAction::Launch,
                payload,
            })
            .map_err(|_| "daemon request failed; reconnect to continue".to_owned())?
        {
            DaemonReply::Ok(body) | DaemonReply::Accepted { body, .. } => body
                .get("terminal")
                .cloned()
                .ok_or_else(|| "terminal launch was not accepted".to_owned())
                .and_then(|terminal| {
                    serde_json::from_value(terminal)
                        .map_err(|_| "terminal launch returned an invalid terminal".to_owned())
                }),
        }
    }

    #[coverage(off)]
    fn attach_terminal(
        &mut self,
        terminal: &usagi_core::domain::id::TerminalRef,
        _geometry: Geometry,
    ) -> Result<TerminalAttach, TerminalError> {
        let body = self.terminal_request(
            TerminalAction::Attach,
            TerminalRequest::Attach {
                terminal: terminal.clone(),
            },
        )?;
        let subscription = body["subscription"]
            .as_u64()
            .ok_or(TerminalError::Unavailable)?;
        let snapshot = &body["snapshot"];
        let output_offset = snapshot["output_offset"]
            .as_u64()
            .ok_or(TerminalError::Unavailable)?;
        let replay = serde_json::from_value(snapshot["replay"].clone()).unwrap_or_default();
        // `exited` is `Option<i32>`: null while the process is still running.
        let exited = !snapshot["exited"].is_null();
        Ok(TerminalAttach {
            subscription,
            output_offset,
            replay,
            exited,
        })
    }

    #[coverage(off)]
    fn resize_terminal(
        &mut self,
        terminal: &usagi_core::domain::id::TerminalRef,
        geometry: Geometry,
    ) -> Result<(), TerminalError> {
        self.terminal_request(
            TerminalAction::Resize,
            TerminalRequest::Resize {
                terminal: terminal.clone(),
                geometry: TerminalGeometry {
                    cols: geometry.cols,
                    rows: geometry.rows,
                },
            },
        )?;
        Ok(())
    }

    #[coverage(off)]
    fn poll_terminal(
        &mut self,
        terminal: &usagi_core::domain::id::TerminalRef,
        after_offset: u64,
    ) -> Result<Vec<TerminalChunk>, TerminalError> {
        let body = self.terminal_request(
            TerminalAction::Resume,
            TerminalRequest::Resume {
                terminal: terminal.clone(),
                after_offset,
            },
        )?;
        let outputs = body["output"].as_array().cloned().unwrap_or_default();
        let mut chunks = Vec::with_capacity(outputs.len());
        for output in outputs {
            let start_offset = output["start_offset"]
                .as_u64()
                .ok_or(TerminalError::Unavailable)?;
            let end_offset = output["end_offset"]
                .as_u64()
                .ok_or(TerminalError::Unavailable)?;
            let data = serde_json::from_value(output["data"].clone()).unwrap_or_default();
            chunks.push(TerminalChunk {
                start_offset,
                end_offset,
                data,
            });
        }
        Ok(chunks)
    }

    #[coverage(off)]
    fn input_terminal(
        &mut self,
        terminal: &usagi_core::domain::id::TerminalRef,
        subscription: u64,
        input_seq: u64,
        bytes: &[u8],
    ) -> Result<(), TerminalError> {
        self.terminal_request(
            TerminalAction::Input,
            TerminalRequest::Input {
                terminal: terminal.clone(),
                subscription,
                input_seq,
                bytes: bytes.to_vec(),
            },
        )?;
        Ok(())
    }

    #[coverage(off)]
    fn detach_terminal(
        &mut self,
        terminal: &usagi_core::domain::id::TerminalRef,
        subscription: u64,
    ) {
        let _ = self.terminal_request(
            TerminalAction::Detach,
            TerminalRequest::Detach {
                terminal: terminal.clone(),
                subscription,
            },
        );
    }
}

struct LifecycleSnapshot {
    workspace_id: WorkspaceId,
    revision: u64,
    sessions: Vec<ManagedSession>,
}

impl LifecycleSnapshot {
    #[coverage(off)]
    fn project(self, workspace: &Workspace) -> Vec<SessionRecord> {
        self.sessions
            .into_iter()
            .map(|session| SessionRecord {
                name: session.name.clone(),
                display_name: None,
                origin: SessionOrigin::Unknown,
                started_from: None,
                root: workspace
                    .path
                    .join(".usagi")
                    .join("sessions")
                    .join(session.name),
                created_at: session.changed_at,
                last_active: None,
                notes: Scratchpad::default(),
                prs: Vec::new(),
            })
            .collect()
    }
}

#[coverage(off)]
fn lifecycle_snapshot(value: &serde_json::Value) -> Result<LifecycleSnapshot, String> {
    let result = (|| {
        let object = value
            .as_object()
            .ok_or_else(|| "invalid daemon session snapshot".to_owned())?;
        let revision = object
            .get("revision")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| "daemon session snapshot has no revision".to_owned())?;
        let workspace_id = object
            .get("workspace_id")
            .cloned()
            .ok_or_else(|| "daemon session snapshot has no workspace ID".to_owned())
            .and_then(|id| {
                serde_json::from_value(id)
                    .map_err(|_| "daemon session snapshot has an invalid workspace ID".to_owned())
            })?;
        let sessions = object
            .get("sessions")
            .cloned()
            .ok_or_else(|| "daemon session snapshot has no sessions".to_owned())
            .and_then(|sessions| {
                serde_json::from_value(sessions)
                    .map_err(|error| format!("invalid daemon session snapshot: {error}"))
            })?;
        Ok(LifecycleSnapshot {
            workspace_id,
            revision,
            sessions,
        })
    })();
    if let Err(error) = &result {
        // The daemon snapshot contains no user-supplied argv or environment.
        // Persist only the schema error, never the raw IPC body.
        ErrorLog::record(&format!("daemon lifecycle snapshot rejected: {error}"));
    }
    result
}

impl SessionCommandPort for DaemonSessionCommandPort {
    #[coverage(off)]
    fn execute(
        &mut self,
        workspace: &Workspace,
        _selected: Option<&usagi_core::domain::session::SessionRecord>,
        command: SessionCommand,
    ) -> Result<SessionCommandResult, String> {
        let (action, payload) = match command {
            SessionCommand::Create { name } => {
                (SessionAction::Create, serde_json::json!({"name": name}))
            }
            SessionCommand::List => (SessionAction::List, serde_json::json!({})),
            SessionCommand::Overview => (SessionAction::Overview, serde_json::json!({})),
            SessionCommand::SelectRemove { .. } => {
                return Err("session selection must be handled by the TUI".to_owned());
            }
            SessionCommand::Remove { name, force } => (
                SessionAction::Remove,
                serde_json::json!({"name": name, "force": force}),
            ),
        };
        let operation_id = usagi_core::domain::id::OperationId::new().to_string();
        let mut client =
            crate::runtime::daemon::client(usagi_core::usecase::client::ClientPolicy::tui())
                .map_err(|error| format!("daemon unavailable: {error}"))?;
        let reply = client
            .request(DaemonRequest::Session {
                action,
                operation_id,
                payload,
            })
            .map_err(daemon_error_reason)?;
        match reply {
            DaemonReply::Accepted {
                operation_id,
                revision,
                body,
            } => {
                let snapshot = lifecycle_snapshot(&body)?;
                if snapshot.revision < self.last_revision {
                    return Ok(SessionCommandResult::message(
                        "ignored stale daemon snapshot",
                    ));
                }
                if action == SessionAction::Create {
                    created_session_hook(&body, &operation_id, revision)?;
                }
                self.last_revision = snapshot.revision;
                Ok(session_snapshot_result(
                    format!("completed operation {operation_id} (revision {revision})"),
                    snapshot,
                    workspace,
                ))
            }
            DaemonReply::Ok(value) => {
                let snapshot = lifecycle_snapshot(&value)?;
                if snapshot.revision < self.last_revision {
                    return Ok(SessionCommandResult::message(
                        "ignored stale daemon snapshot",
                    ));
                }
                self.last_revision = snapshot.revision;
                Ok(session_snapshot_result(
                    "daemon snapshot refreshed",
                    snapshot,
                    workspace,
                ))
            }
        }
    }
}

/// Validate the daemon-owned final hook that ends a `session create` loading
/// wave.  A snapshot by itself is not sufficient here: a delayed or unrelated
/// accepted response must not clear the pending skeleton for this operation.
#[coverage(off)]
fn created_session_hook(
    value: &serde_json::Value,
    operation_id: &str,
    revision: u64,
) -> Result<(), String> {
    let hook = value
        .get("hook")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "daemon create completion hook is missing".to_owned())?;
    let kind = hook.get("kind").and_then(serde_json::Value::as_str);
    let hook_operation = hook.get("operation_id").and_then(serde_json::Value::as_str);
    let hook_revision = hook.get("revision").and_then(serde_json::Value::as_u64);
    if kind == Some("session.created")
        && hook_operation == Some(operation_id)
        && hook_revision == Some(revision)
    {
        Ok(())
    } else {
        Err("daemon create completion hook does not match the operation".to_owned())
    }
}

#[coverage(off)]
fn session_snapshot_result(
    message: impl Into<String>,
    snapshot: LifecycleSnapshot,
    workspace: &Workspace,
) -> SessionCommandResult {
    let session_ids = snapshot
        .sessions
        .iter()
        .map(|session| session.session_id)
        .collect();
    SessionCommandResult {
        message: message.into(),
        sessions: Some(snapshot.project(workspace)),
        session_ids: Some(session_ids),
    }
}

/// Overview の session command port を workspace 起動ごとに新しく作る合成側 factory。
///
/// screen graph（Welcome→Open / Recent）は 1 ループで複数の workspace を順に開くため、
/// daemon の revision state を持ち越さないよう port を都度生成する。
struct DaemonSessionCommandPortFactory;

impl SessionCommandPortFactory for DaemonSessionCommandPortFactory {
    #[coverage(off)]
    fn create(&mut self) -> Box<dyn SessionCommandPort> {
        Box::new(DaemonSessionCommandPort::default())
    }
}

#[coverage(off)]
fn request_lifecycle_snapshot() -> Result<LifecycleSnapshot, String> {
    let mut client =
        crate::runtime::daemon::client(usagi_core::usecase::client::ClientPolicy::tui())
            .map_err(|error| format!("daemon unavailable: {error}"))?;
    match client
        .request(DaemonRequest::Session {
            action: SessionAction::List,
            operation_id: usagi_core::domain::id::OperationId::new().to_string(),
            payload: serde_json::json!({}),
        })
        .map_err(daemon_error_reason)?
    {
        DaemonReply::Ok(value) => lifecycle_snapshot(&value),
        DaemonReply::Accepted { .. } => {
            Err("daemon returned an invalid lifecycle snapshot response".to_owned())
        }
    }
}

/// Render only the user-actionable daemon reason in the TUI.  Error codes and
/// transport variant labels remain useful to diagnostics but add no context to
/// an interactive failure notice.
#[coverage(off)]
fn daemon_error_reason(error: ClientError) -> String {
    match error {
        ClientError::Protocol(error) => error.message,
        ClientError::Unavailable(message) | ClientError::Lifecycle(message) => message,
    }
}

struct CrosstermTerminal {
    out: std::io::Stdout,
    input: EventPump<CrosstermSource, NoBackend<()>>,
    input_started: Instant,
    renderer: FrameRenderer,
    /// live-terminal `Ctrl-O` prefix の SSoT。leader を保持して follow-up を
    /// [`Key::Live`] へ翻訳する。`Ctrl-O` 以外は passthrough として従来の
    /// `Key` マッピングに委ねるため、live terminal への passthrough を壊さない。
    live_input: LiveInputClassifier,
    /// The concrete OS adapter is owned by the composition root. Selection
    /// commands receive it through the TUI clipboard port rather than creating
    /// subprocesses in presentation code.
    clipboard: PlatformClipboard,
}

struct PersistentSettingsPort {
    storage: Storage,
    workspace: Settings,
}

impl PersistentSettingsPort {
    fn open() -> std::io::Result<Self> {
        Ok(Self {
            storage: Storage::open_default().map_err(io_error)?,
            workspace: Settings::default(),
        })
    }
}

impl SettingsPort for PersistentSettingsPort {
    #[coverage(off)]
    fn read(&mut self, scope: SettingsScope) -> std::io::Result<Settings> {
        Ok(match scope {
            SettingsScope::Global => self.storage.load_settings().map_err(io_error)?,
            SettingsScope::Workspace => self.workspace.clone(),
        })
    }

    #[coverage(off)]
    fn save(&mut self, scope: SettingsScope, settings: &Settings) -> std::io::Result<()> {
        match scope {
            SettingsScope::Global => {
                let _lock = self.storage.lock().map_err(io_error)?;
                self.storage.save_settings(settings).map_err(io_error)?;
            }
            SettingsScope::Workspace => self.workspace = settings.clone(),
        }
        Ok(())
    }
}

impl Terminal for CrosstermTerminal {
    #[coverage(off)]
    fn size(&mut self) -> std::io::Result<(usize, usize)> {
        let (cols, rows) = terminal::size()?;
        Ok((rows as usize, cols as usize))
    }

    #[coverage(off)]
    fn draw(&mut self, frame: &[String]) -> std::io::Result<()> {
        let (height, width) = self.size()?;
        let diff = self
            .renderer
            .render(Frame::from_lines(width, height, frame));
        if diff.clear_surface {
            queue!(
                self.out,
                cursor::MoveTo(0, 0),
                terminal::Clear(terminal::ClearType::All)
            )?;
        }
        for span in diff.spans {
            queue!(
                self.out,
                cursor::MoveTo(
                    u16::try_from(span.column).expect("terminal width came from crossterm"),
                    u16::try_from(span.row).expect("terminal height came from crossterm")
                )
            )?;
            write!(self.out, "{}", span.text)?;
        }
        if let Some((row, column)) = diff.input_cursor {
            queue!(
                self.out,
                cursor::MoveTo(
                    u16::try_from(column).expect("terminal width came from crossterm"),
                    u16::try_from(row).expect("terminal height came from crossterm")
                )
            )?;
        }
        self.out.flush()
    }

    #[coverage(off)]
    fn wait(&mut self, duration: Duration) -> std::io::Result<()> {
        std::thread::sleep(duration);
        Ok(())
    }

    #[coverage(off)]
    fn read_key(&mut self) -> std::io::Result<Key> {
        loop {
            match self.input.next(self.input_started.elapsed())? {
                RuntimeEvent::Input(input) => {
                    if let LiveInput::Pointer(pointer) = input {
                        return Ok(Key::Pointer(pointer));
                    }
                    if let LiveInput::Mouse { column, row } = input {
                        return Ok(Key::Click { column, row });
                    }
                    // Global control chords stay outside the live-prefix classifier.
                    if let Some(key) = control_key(&input) {
                        return Ok(key);
                    }
                    let now = self.input_started.elapsed();
                    match self.live_input.classify(now, input.clone()) {
                        // A resolved `Ctrl-O` prefix action drives the live runtime.
                        LiveInputOutput::Action(action) => return Ok(Key::Live(action)),
                        // Leader pending, unknown follow-up, or key release: keep reading.
                        LiveInputOutput::Swallowed => {}
                        // Everything else is a normal key; preserve the prior mapping so
                        // non-prefix keys and future PTY passthrough are unchanged.
                        LiveInputOutput::Passthrough(bytes) => {
                            return Ok(passthrough_key(&input, bytes));
                        }
                    }
                }
                RuntimeEvent::Resize { .. } => {
                    self.renderer.reset_surface();
                    return Ok(Key::Other);
                }
                // Tick wakes the TUI while a background session command owns
                // the daemon port, so the pending skeleton can redraw.
                RuntimeEvent::Backend(()) | RuntimeEvent::Tick => return Ok(Key::Other),
            }
        }
    }

    #[coverage(off)]
    fn copy_text(&mut self, text: &str) -> Result<(), String> {
        use usagi_tui::usecase::application::terminal_selection::ClipboardPort;
        self.clipboard.write_text(text)
    }
}

/// Map global control chords before their bytes can reach a text field.
///
/// [`Key::CtrlD`] has an effect only in Open Workspace; other screens explicitly
/// ignore it. Keeping it as a dedicated key prevents a U+0004 control character
/// from being inserted into their inputs.
#[coverage(off)]
fn control_key(input: &LiveInput) -> Option<Key> {
    let LiveInput::Key(key) = input else {
        return None;
    };
    (key.modifiers.control
        && !key.modifiers.shift
        && !key.modifiers.alt
        && !key.modifiers.super_
        && !key.modifiers.hyper
        && !key.modifiers.meta)
        .then_some(match key.code {
            KeyCode::Char('c') => Some(Key::Quit),
            KeyCode::Char('q') => Some(Key::CtrlQ),
            KeyCode::Char('d') => Some(Key::CtrlD),
            _ => None,
        })?
}

/// Map a non-prefix live input to the management `Key` vocabulary. The classifier
/// has already reserved the `Ctrl-O` prefix, so this preserves the prior mapping
/// for every other key and text/paste payload.
#[coverage(off)]
fn passthrough_key(input: &LiveInput, bytes: Vec<u8>) -> Key {
    let key = match input {
        LiveInput::Key(key) => key,
        // Some terminal decoders preserve Return as its original byte instead
        // of emitting a semantic key event. Management modals must accept both
        // forms, otherwise Closeup actions appear to ignore Enter.
        LiveInput::Raw(bytes) if bytes.as_slice() == b"\r" || bytes.as_slice() == b"\n" => {
            return Key::Enter;
        }
        LiveInput::Text(text) if text == "\r" || text == "\n" => {
            return Key::Enter;
        }
        LiveInput::Raw(_) | LiveInput::Text(_) | LiveInput::Paste(_) => {
            return Key::Passthrough(bytes);
        }
        LiveInput::Mouse { .. }
        | LiveInput::WheelUp
        | LiveInput::WheelDown
        | LiveInput::Pointer(_) => return Key::Other,
    };
    // Some terminal backends report an auto-repeat as the first observable
    // key event.  Treat it like a press so management controls (notably
    // Closeup's Enter action) are never dropped; only releases are inert.
    if matches!(key.kind, KeyEventKind::Release) {
        return Key::Other;
    }
    // Ctrl-A is the IME-safe shortcut for the persistent `+ new session`
    // action. Preserve it as a control byte so typing `c` directly on the
    // action row can still start a session name with that character.
    if (key.modifiers.control && key.code == KeyCode::Char('a'))
        || key.code == KeyCode::Char('\u{1}')
    {
        return Key::Char('\u{1}');
    }
    // The live classifier has already encoded the original terminal input.
    // Keep modified chords opaque so this management-key adapter cannot drop
    // their Ctrl/Alt bytes before Closeup forwards them to the focused pane.
    if key.modifiers != Modifiers::default() {
        return Key::Passthrough(bytes);
    }
    match key.code {
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Enter => Key::Enter,
        KeyCode::Tab => Key::Tab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Escape => Key::Escape,
        KeyCode::Char(ch) => Key::Char(ch),
        _ => Key::Other,
    }
}

#[coverage(off)]
fn io_error(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

#[coverage(off)]
pub(crate) fn resolve_workspace_path(path: &Path) -> std::io::Result<PathBuf> {
    let resolved = std::fs::canonicalize(path)?;
    validate_workspace_directory(&resolved)?;
    Ok(resolved)
}

#[coverage(off)]
fn validate_workspace_directory(path: &Path) -> std::io::Result<()> {
    if !std::fs::metadata(path)?.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("workspace path is not a directory: {}", path.display()),
        ));
    }
    Ok(())
}

fn load_workspace_state(
    path: &Path,
) -> std::io::Result<usagi_core::domain::workspace_state::WorkspaceState> {
    WorkspaceStateStore::new(path)
        .load()
        .map_err(io_error)
        .map(Option::unwrap_or_default)
}

struct FsWorkspaceLoader {
    storage: Storage,
}

impl FsWorkspaceLoader {
    #[coverage(off)]
    fn open_default() -> std::io::Result<Self> {
        Ok(Self {
            storage: Storage::open_default().map_err(io_error)?,
        })
    }
}

impl WorkspaceLoader for FsWorkspaceLoader {
    #[coverage(off)]
    fn open(&mut self, path: &Path) -> std::io::Result<WorkspaceSnapshot> {
        validate_workspace_directory(path)?;
        let workspace =
            workspace_usecase::open(&self.storage, path, Utc::now()).map_err(io_error)?;
        let mut state = load_workspace_state(&workspace.path)?;
        let lifecycle = request_lifecycle_snapshot().map_err(io_error)?;
        let workspace_id = lifecycle.workspace_id;
        let session_ids = lifecycle
            .sessions
            .iter()
            .map(|session| session.session_id)
            .collect();
        state.sessions = lifecycle.project(&workspace);
        Ok(WorkspaceSnapshot::with_runtime_ids(
            workspace,
            state,
            workspace_id,
            session_ids,
        ))
    }

    #[coverage(off)]
    fn cleanup_missing(&mut self, workspaces: &[Workspace]) -> std::io::Result<Vec<PathBuf>> {
        let missing = workspaces
            .iter()
            .filter(|workspace| !workspace.path.is_dir())
            .map(|workspace| workspace.path.clone())
            .collect::<Vec<_>>();
        Ok(workspace_usecase::remove(&self.storage, &missing)
            .map_err(io_error)?
            .into_iter()
            .map(|workspace| workspace.path)
            .collect())
    }

    #[coverage(off)]
    fn unregister(&mut self, paths: &[PathBuf]) -> std::io::Result<Vec<PathBuf>> {
        Ok(workspace_usecase::remove(&self.storage, paths)
            .map_err(io_error)?
            .into_iter()
            .map(|workspace| workspace.path)
            .collect())
    }
}

#[coverage(off)]
fn load_screen_graph_data(
    storage: &Storage,
    start: Start,
) -> std::io::Result<(Vec<Workspace>, Vec<Recent>)> {
    match start {
        Start::Welcome => Ok((
            storage.load_workspaces().map_err(io_error)?,
            workspace_usecase::recent(storage).map_err(io_error)?,
        )),
        Start::Config => Ok((
            storage.load_workspaces().unwrap_or_default(),
            workspace_usecase::recent(storage).unwrap_or_default(),
        )),
    }
}

#[coverage(off)]
fn run_in_terminal(
    run: impl FnOnce(&mut CrosstermTerminal) -> std::io::Result<Exit>,
) -> std::io::Result<Exit> {
    enable_raw_mode()?;
    let mut setup = std::io::stdout();
    if let Err(error) = execute!(
        setup,
        EnterAlternateScreen,
        EnableMouseCapture,
        terminal::DisableLineWrap,
        cursor::Hide
    ) {
        let _ = execute!(
            setup,
            cursor::Show,
            terminal::EnableLineWrap,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
        return Err(error);
    }
    let mut terminal = CrosstermTerminal {
        out: std::io::stdout(),
        input: EventPump::new(
            CrosstermSource,
            NoBackend::default(),
            Duration::from_millis(16),
            Duration::ZERO,
        ),
        input_started: Instant::now(),
        renderer: FrameRenderer::new(),
        live_input: LiveInputClassifier::default(),
        clipboard: PlatformClipboard,
    };
    let result = run(&mut terminal);
    let mut teardown = std::io::stdout();
    let _ = execute!(
        teardown,
        cursor::Show,
        terminal::EnableLineWrap,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
    result
}

/// Keeps the daemon metrics observer alive for exactly one interactive TUI
/// lifetime.  A fresh connection-local subscription is created on every TUI
/// launch; orderly teardown explicitly unregisters it.
#[coverage(off)]
fn run_with_metrics_hook(run: impl FnOnce() -> std::io::Result<Exit>) -> std::io::Result<Exit> {
    let mut hook = MetricsHook::default();
    let mut client = crate::runtime::daemon::client(ClientPolicy::tui()).map_err(io_error)?;
    hook.connect(&mut client).map_err(io_error)?;
    let result = run();
    let cleanup = hook.shutdown(&mut client).map_err(io_error);
    match result {
        Ok(exit) => cleanup.map(|()| exit),
        Err(error) => {
            let _ = cleanup;
            Err(error)
        }
    }
}

#[coverage(off)]
fn launch_screen_graph(out: &mut dyn Write, start: Start) -> std::io::Result<()> {
    let now = Utc::now();
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        let storage = Storage::open_default().map_err(io_error)?;
        let (workspaces, recent) = load_screen_graph_data(&storage, start)?;
        let mut loader = FsWorkspaceLoader { storage };
        let mut settings = PersistentSettingsPort::open()?;
        let mut session_commands = DaemonSessionCommandPortFactory;
        let mut agent_commands = DaemonAgentCommandPortFactory;
        let mut metrics = DaemonMetricsPortFactory;
        run_with_metrics_hook(|| {
            run_in_terminal(|terminal| {
                if start == Start::Welcome {
                    presentation::play_startup_splash(terminal)?;
                }
                presentation::run_with_settings_and_agent_and_metrics_port_factory_and_model_availability(
                    terminal,
                    workspaces,
                    recent,
                    now,
                    start,
                    &mut loader,
                    &mut settings,
                    &mut session_commands,
                    &mut agent_commands,
                    available_agent_models(),
                    &mut metrics,
                )
            })
        })?;
    } else {
        let frame = match start {
            Start::Welcome => {
                let storage = Storage::open_default().map_err(io_error)?;
                welcome::render(
                    0,
                    0,
                    &Welcome::new(workspace_usecase::recent(&storage).map_err(io_error)?),
                    now,
                )
            }
            Start::Config => {
                let mut settings = PersistentSettingsPort::open()?;
                config::render(
                    0,
                    0,
                    &Config::load_with_available_models(&mut settings, available_agent_models()),
                )
            }
        };
        for line in frame {
            writeln!(out, "{line}")?;
        }
    }
    Ok(())
}

#[coverage(off)]
fn available_agent_models() -> AvailableAgentModels {
    AvailableAgentModels::new(cli_is_available("claude"), cli_is_available("codex"))
}

#[coverage(off)]
fn cli_is_available(program: &str) -> bool {
    Command::new(program).arg("--version").output().is_ok()
}

#[coverage(off)]
fn launch_workspace(out: &mut dyn Write, path: &Path) -> std::io::Result<()> {
    let mut loader = FsWorkspaceLoader::open_default()?;
    let snapshot = loader.open(path)?;
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        let mut settings = PersistentSettingsPort::open()?;
        let global_settings = settings.read(SettingsScope::Global)?;
        run_with_metrics_hook(|| {
            run_in_terminal(|terminal| {
                presentation::run_workspace_with_agent_port_and_selection_mode(
                    terminal,
                    snapshot,
                    Box::new(DaemonSessionCommandPort::default()),
                    global_settings.modal_selection_mode,
                    global_settings.default_model,
                    Box::new(DaemonAgentCommandPort::new()),
                    Box::new(DaemonMetricsPort::new()),
                )
            })
        })?;
    } else {
        let workspace = WorkspaceView::new(snapshot.workspace, snapshot.state);
        for line in workspace::render(0, 0, &workspace) {
            writeln!(out, "{line}")?;
        }
    }
    Ok(())
}

#[coverage(off)]
pub(crate) fn launch(
    out: &mut dyn Write,
    info: &AppInfo,
    entry: &EntryScreen,
) -> std::io::Result<()> {
    if let Err(error) = crate::runtime::daemon::ensure_ready() {
        writeln!(std::io::stderr(), "daemon unavailable: {error}")?;
        return Ok(());
    }
    match entry {
        EntryScreen::Welcome => launch_screen_graph(out, Start::Welcome),
        EntryScreen::Config => launch_screen_graph(out, Start::Config),
        EntryScreen::Workspace { path } => launch_workspace(out, path),
        EntryScreen::Doctor => {
            let mut runner = BannerScreenRunner::new(out, info);
            application::run(entry, &mut runner)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PersistentSettingsPort, Start, control_key, created_session_hook, load_screen_graph_data,
        load_workspace_state, passthrough_key,
    };
    use usagi_core::domain::settings::{ModalSelectionMode, Settings};
    use usagi_core::infrastructure::store::workspace::Storage;
    use usagi_core::usecase::settings::{SettingsPort, SettingsScope};
    use usagi_tui::usecase::application::Key;
    use usagi_tui::usecase::terminal_input::{
        KeyCode, KeyEvent, KeyEventKind, LiveInput, Modifiers,
    };

    #[test]
    fn ctrl_a_maps_to_the_new_session_shortcut() {
        let key = LiveInput::Key(KeyEvent::new(
            KeyCode::Char('a'),
            Modifiers {
                control: true,
                ..Modifiers::default()
            },
            KeyEventKind::Press,
        ));
        assert_eq!(passthrough_key(&key, Vec::new()), Key::Char('\u{1}'));
    }

    #[test]
    fn ctrl_d_maps_to_the_dedicated_open_workspace_unregister_key() {
        let key = LiveInput::Key(KeyEvent::new(
            KeyCode::Char('d'),
            Modifiers {
                control: true,
                ..Modifiers::default()
            },
            KeyEventKind::Press,
        ));
        assert_eq!(control_key(&key), Some(Key::CtrlD));
    }

    #[test]
    fn ctrl_c_maps_to_quit_so_a_live_terminal_receives_sigint() {
        let key = LiveInput::Key(KeyEvent::new(
            KeyCode::Char('c'),
            Modifiers {
                control: true,
                ..Modifiers::default()
            },
            KeyEventKind::Press,
        ));
        assert_eq!(control_key(&key), Some(Key::Quit));
    }

    #[test]
    fn modified_non_leader_keys_keep_their_terminal_bytes() {
        let ctrl_r = LiveInput::Key(KeyEvent::new(
            KeyCode::Char('r'),
            Modifiers {
                control: true,
                ..Modifiers::default()
            },
            KeyEventKind::Press,
        ));
        assert_eq!(
            passthrough_key(&ctrl_r, vec![0x12]),
            Key::Passthrough(vec![0x12])
        );

        let alt_f = LiveInput::Key(KeyEvent::new(
            KeyCode::Char('f'),
            Modifiers {
                alt: true,
                ..Modifiers::default()
            },
            KeyEventKind::Press,
        ));
        assert_eq!(
            passthrough_key(&alt_f, b"\x1bf".to_vec()),
            Key::Passthrough(b"\x1bf".to_vec())
        );
    }

    #[test]
    fn repeat_enter_reaches_the_closeup_action_handler() {
        let key = LiveInput::Key(KeyEvent::new(
            KeyCode::Enter,
            Modifiers::default(),
            KeyEventKind::Repeat,
        ));

        assert_eq!(passthrough_key(&key, Vec::new()), Key::Enter);
    }

    #[test]
    fn released_enter_does_not_repeat_the_closeup_action() {
        let key = LiveInput::Key(KeyEvent::new(
            KeyCode::Enter,
            Modifiers::default(),
            KeyEventKind::Release,
        ));

        assert_eq!(passthrough_key(&key, Vec::new()), Key::Other);
    }

    #[test]
    fn raw_return_reaches_the_closeup_action_handler() {
        for input in [
            LiveInput::Raw(b"\r".to_vec()),
            LiveInput::Text("\n".to_owned()),
        ] {
            assert_eq!(passthrough_key(&input, Vec::new()), Key::Enter);
        }
    }

    #[test]
    fn non_key_terminal_payloads_preserve_their_original_bytes() {
        let payload = b"\x1b[200~paste\x1b[201~".to_vec();
        assert_eq!(
            passthrough_key(&LiveInput::Paste(payload.clone()), payload.clone()),
            Key::Passthrough(payload)
        );
    }

    #[test]
    fn create_loading_ends_only_on_the_matching_daemon_hook() {
        let hook = serde_json::json!({
            "hook": {
                "kind": "session.created",
                "operation_id": "op-1",
                "revision": 7,
            },
        });
        assert!(created_session_hook(&hook, "op-1", 7).is_ok());
        assert!(created_session_hook(&hook, "op-2", 7).is_err());
        assert!(created_session_hook(&hook, "op-1", 8).is_err());
        assert!(created_session_hook(&serde_json::json!({}), "op-1", 7).is_err());
    }

    #[test]
    #[coverage(off)]
    fn config_start_degrades_a_broken_workspace_registry() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("workspaces.json"), "{ broken").unwrap();
        let storage = Storage::new(home.path());
        let (workspaces, recent) = load_screen_graph_data(&storage, Start::Config).unwrap();
        assert!(workspaces.is_empty());
        assert!(recent.is_empty());
        assert!(load_screen_graph_data(&storage, Start::Welcome).is_err());
    }

    #[test]
    fn workspace_state_loader_defaults_only_when_state_is_missing() {
        let workspace = tempfile::tempdir().unwrap();

        let state = load_workspace_state(workspace.path()).unwrap();
        assert!(state.sessions.is_empty());
        assert!(state.root_notes.note.is_none());
        assert!(state.root_notes.todos.is_empty());
        assert!(state.root_notes.decisions.is_empty());
    }

    #[test]
    fn workspace_state_loader_surfaces_a_malformed_state_file() {
        let workspace = tempfile::tempdir().unwrap();
        let state_dir = workspace.path().join(".usagi");
        std::fs::create_dir(&state_dir).unwrap();
        std::fs::write(state_dir.join("state.json"), "{ broken").unwrap();

        let error = load_workspace_state(workspace.path()).unwrap_err();
        assert!(error.to_string().contains("state.json"));
    }

    #[test]
    fn global_modal_mode_survives_a_new_tui_settings_port() {
        let temporary = tempfile::tempdir().unwrap();
        let storage = Storage::new(temporary.path());
        let mut first = PersistentSettingsPort {
            storage: Storage::new(temporary.path()),
            workspace: Settings::default(),
        };
        let settings = Settings {
            modal_selection_mode: ModalSelectionMode::Prompt,
            ..Settings::default()
        };
        first.save(SettingsScope::Global, &settings).unwrap();
        let mut restarted = PersistentSettingsPort {
            storage,
            workspace: Settings::default(),
        };
        assert_eq!(
            restarted
                .read(SettingsScope::Global)
                .unwrap()
                .modal_selection_mode,
            ModalSelectionMode::Prompt
        );
    }
}

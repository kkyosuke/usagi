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
use usagi_core::domain::id::{UserDecisionId, WorkspaceId};
use usagi_core::domain::note::Scratchpad;
use usagi_core::domain::recent::Recent;
use usagi_core::domain::session::{SessionOrigin, SessionRecord};
use usagi_core::domain::session_lifecycle::ManagedSession;
use usagi_core::domain::settings::Settings;
use usagi_core::domain::terminal_launch::{
    TerminalLaunchRequest, TerminalLaunchScope, TerminalProfileId,
};
use usagi_core::domain::user_decision::UserDecisionAnswer;
use usagi_core::domain::workspace::Workspace;
use usagi_core::infrastructure::error_log::ErrorLog;
use usagi_core::infrastructure::git::{clone as git_clone, diff_status};
use usagi_core::infrastructure::store::state::WorkspaceStateStore;
use usagi_core::infrastructure::store::workspace::Storage;
use usagi_core::usecase::client::{
    AgentLaunchIntent, ClientError, ClientPolicy, DaemonClient, DaemonMetrics, DaemonReply,
    DaemonRequest, IpcClient, MetricsAction, PrAction, PrRequest, SessionAction, TerminalAction,
    TerminalGeometry, TerminalLaunchIntent, TerminalRequest,
};
use usagi_core::usecase::settings::{SettingsPort, SettingsScope};
use usagi_core::usecase::workspace as workspace_usecase;
use usagi_daemon::usecase::session_runtime::SystemGit;
use usagi_tui::infrastructure::metrics::MetricsHook;
use usagi_tui::presentation::frame::{Frame, FrameRenderer};
use usagi_tui::presentation::views::config::{self, AvailableAgentModels, Config};
use usagi_tui::presentation::views::welcome::{self, Welcome};
use usagi_tui::presentation::views::workspace::GitDiff;
use usagi_tui::presentation::{
    self, AgentCommandPort, AgentCommandPortFactory, BannerScreenRunner, DecisionCommandPort,
    DesktopNotificationPort, Exit, MetricsPort, MetricsPortFactory, SessionCommandPort,
    SessionCommandPortFactory, SessionCommandResult, Start, WorkspaceLoader, WorkspaceSnapshot,
};
use usagi_tui::usecase::application::controller::{
    BackendEvent, NewRequest, Notice, SafeError, SafeMessage,
};
use usagi_tui::usecase::application::pane_runtime::Geometry;
use usagi_tui::usecase::application::pr::{BrowserOpener, PrSnapshotPort};
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

/// Production bridge for the controller's durable user-decision effects.
/// The daemon remains the authority; this adapter only converts its safe
/// snapshots and confirmations back into reducer events.
struct DaemonDecisionCommandPort;

/// Platform delivery is deliberately best-effort.  Fixed executable names and
/// argument-vector spawning keep decision text out of a shell; a missing
/// notification service must never stop the TUI.
struct PlatformDesktopNotifier;

impl DesktopNotificationPort for PlatformDesktopNotifier {
    #[coverage(off)]
    fn notify(&mut self, title: &str, body: &str) {
        let mut command = if cfg!(target_os = "macos") {
            let mut command = Command::new("osascript");
            command
                .arg("-e")
                .arg("on run argv\n display notification (item 2 of argv) with title (item 1 of argv)\nend run")
                .arg("--")
                .arg(title)
                .arg(body);
            command
        } else if cfg!(target_os = "linux") {
            let mut command = Command::new("notify-send");
            command.arg("--app-name=usagi").arg(title).arg(body);
            command
        } else {
            return;
        };
        let _ = command.spawn();
    }
}

impl DaemonDecisionCommandPort {
    #[coverage(off)]
    fn client() -> Result<impl DaemonClient, String> {
        crate::runtime::daemon::client(ClientPolicy::tui())
            .map_err(|error| format!("daemon unavailable: {error}"))
    }

    #[coverage(off)]
    fn safe_error(error: impl std::fmt::Display) -> SafeError {
        SafeError {
            message: SafeMessage::new(error.to_string()),
            error_id: "decision-daemon-error".to_owned(),
        }
    }
}

impl DecisionCommandPort for DaemonDecisionCommandPort {
    #[coverage(off)]
    fn refresh(&mut self, workspace: WorkspaceId) -> BackendEvent {
        let result =
            (|| -> Result<Vec<usagi_core::domain::user_decision::UserDecision>, String> {
                let mut client = Self::client()?;
                let reply = client
                    .request(DaemonRequest::UserDecision {
                        action: usagi_core::usecase::client::TuiUserDecisionAction::List,
                        payload: serde_json::json!({}),
                    })
                    .map_err(daemon_error_reason)?;
                let DaemonReply::Ok(value) = reply else {
                    return Err("daemon did not return a decision snapshot".to_owned());
                };
                serde_json::from_value(value.get("decisions").cloned().unwrap_or(value))
                    .map_err(|_| "daemon returned an invalid decision snapshot".to_owned())
            })();
        match result {
            Ok(decisions) => BackendEvent::Decisions {
                workspace,
                decisions,
            },
            Err(error) => BackendEvent::Notice(Notice::new(error)),
        }
    }

    #[coverage(off)]
    fn resolve(
        &mut self,
        workspace: WorkspaceId,
        decision_id: UserDecisionId,
        answer: UserDecisionAnswer,
    ) -> BackendEvent {
        let result = (|| -> Result<(), String> {
            let mut client = Self::client()?;
            match client
                .request(DaemonRequest::UserDecision {
                    action: usagi_core::usecase::client::TuiUserDecisionAction::Resolve,
                    payload: serde_json::json!({"decision_id": decision_id, "answer": answer}),
                })
                .map_err(daemon_error_reason)?
            {
                DaemonReply::Ok(_) => Ok(()),
                DaemonReply::Accepted { .. } => {
                    Err("daemon did not confirm the decision answer".to_owned())
                }
            }
        })();
        match result {
            Ok(()) => BackendEvent::DecisionResolved {
                workspace,
                decision_id,
            },
            Err(error) => BackendEvent::DecisionError {
                workspace,
                decision_id,
                error: Self::safe_error(error),
            },
        }
    }
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
        session: Option<usagi_core::domain::id::SessionId>,
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
        session: Option<usagi_core::domain::id::SessionId>,
        geometry: usagi_tui::usecase::application::pane_runtime::Geometry,
    ) -> Result<usagi_core::domain::id::TerminalRef, String> {
        let lifecycle = request_lifecycle_snapshot()
            .map_err(|_| "daemon unavailable; reconnect to continue".to_owned())?;
        if lifecycle.workspace_id != workspace {
            return Err("workspace is no longer available".to_owned());
        }
        // The client never supplies a path or invents a worktree identity: the
        // worktree ID comes from the daemon snapshot (the session's for a session
        // scope, the published root worktree for the workspace root) and the
        // daemon re-validates it before resolving the trusted checkout path.
        let worktree_id = match session {
            None => lifecycle.root_worktree_id,
            Some(session) => {
                let managed = lifecycle
                    .sessions
                    .iter()
                    .find(|candidate| candidate.session_id == session)
                    .ok_or_else(|| "selected session is no longer available".to_owned())?;
                if managed.lifecycle
                    != usagi_core::domain::session_lifecycle::SessionLifecycle::Available
                {
                    return Err("selected session is not ready for a terminal".to_owned());
                }
                managed.worktree_id
            }
        };
        let intent = TerminalLaunchIntent {
            request: TerminalLaunchRequest {
                profile_id: TerminalProfileId::new("login-shell")
                    .expect("static terminal profile is valid"),
                scope: TerminalLaunchScope {
                    workspace_id: workspace,
                    session_id: session,
                    worktree_id,
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
    fn list_terminals(
        &mut self,
    ) -> Result<
        Vec<usagi_core::domain::terminal_launch::TerminalInventoryEntry>,
        usagi_tui::usecase::application::terminal_session::TerminalError,
    > {
        use usagi_core::domain::session_lifecycle::SessionLifecycle;
        use usagi_core::domain::terminal_launch::{TerminalInventoryEntry, TerminalLaunchScope};
        use usagi_tui::usecase::application::terminal_session::TerminalError;

        let lifecycle = request_lifecycle_snapshot().map_err(|_| TerminalError::Unavailable)?;
        // The client never invents a worktree identity: scopes come from the
        // daemon snapshot — the published root worktree and each available
        // session's worktree — and the daemon re-validates them. Sessions the
        // snapshot does not list as available are skipped, so a stale or
        // recreated session's old runtime is never rediscovered.
        let mut scopes = vec![TerminalLaunchScope {
            workspace_id: lifecycle.workspace_id,
            session_id: None,
            worktree_id: lifecycle.root_worktree_id,
        }];
        for managed in &lifecycle.sessions {
            if managed.lifecycle == SessionLifecycle::Available {
                scopes.push(TerminalLaunchScope {
                    workspace_id: lifecycle.workspace_id,
                    session_id: Some(managed.session_id),
                    worktree_id: managed.worktree_id,
                });
            }
        }
        let mut entries = Vec::new();
        for scope in scopes {
            let body = self.terminal_request(
                TerminalAction::Inventory,
                TerminalRequest::Inventory { scope },
            )?;
            if let Some(list) = body.get("terminals").and_then(|value| value.as_array()) {
                for item in list {
                    if let Ok(entry) =
                        serde_json::from_value::<TerminalInventoryEntry>(item.clone())
                    {
                        entries.push(entry);
                    }
                }
            }
        }
        Ok(entries)
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
        decode_terminal_poll(&body)
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

/// Decode a terminal `Resume` reply into the output chunks a session applies.
///
/// The daemon reports the hosting process's exit in the same reply
/// (`"exited": true`) for both generic terminals and Agent runtimes. Once no
/// further output remains to apply, that exit is surfaced as
/// [`TerminalError::Exited`] so the per-frame poll — not only an incidental
/// resync — transitions the [`usagi_tui`] terminal session to exited and the
/// Closeup pane tab is dropped. A reply that still carries fresh output yields
/// the chunks first; the next poll (which returns no new output) then reports
/// the exit, preserving the final output before the tab disappears.
fn decode_terminal_poll(body: &serde_json::Value) -> Result<Vec<TerminalChunk>, TerminalError> {
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
    // `exited` is absent while running (and on daemons that omit it), so only an
    // explicit `true` — after the final output is drained — ends the session.
    if chunks.is_empty() && body["exited"].as_bool() == Some(true) {
        return Err(TerminalError::Exited);
    }
    Ok(chunks)
}

struct LifecycleSnapshot {
    workspace_id: WorkspaceId,
    root_worktree_id: usagi_core::domain::id::WorktreeId,
    revision: u64,
    sessions: Vec<ManagedSession>,
}

impl LifecycleSnapshot {
    #[coverage(off)]
    fn available_sessions(&self) -> impl Iterator<Item = &ManagedSession> {
        self.sessions.iter().filter(|session| {
            session.lifecycle == usagi_core::domain::session_lifecycle::SessionLifecycle::Available
        })
    }

    #[coverage(off)]
    fn project(&self, workspace: &Workspace, legacy: &[SessionRecord]) -> Vec<SessionRecord> {
        self.available_sessions()
            .map(|session| {
                // Lifecycle is daemon-authoritative, but `state.json` remains
                // the durable home of UI-only annotations.  Retain a matching
                // record wholesale and only replace its physical identity.
                let mut record = legacy
                    .iter()
                    .find(|record| record.name == session.name)
                    .cloned()
                    .unwrap_or_else(|| SessionRecord {
                        name: session.name.clone(),
                        display_name: None,
                        origin: SessionOrigin::Unknown,
                        started_from: None,
                        root: workspace
                            .path
                            .join(".usagi")
                            .join("sessions")
                            .join(&session.name),
                        created_at: session.changed_at,
                        last_active: None,
                        notes: Scratchpad::default(),
                        prs: Vec::new(),
                    });
                record.root = workspace
                    .path
                    .join(".usagi")
                    .join("sessions")
                    .join(&session.name);
                record
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
        let root_worktree_id = object
            .get("root_worktree_id")
            .cloned()
            .ok_or_else(|| "daemon session snapshot has no root worktree ID".to_owned())
            .and_then(|id| {
                serde_json::from_value(id).map_err(|_| {
                    "daemon session snapshot has an invalid root worktree ID".to_owned()
                })
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
            root_worktree_id,
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
                session_snapshot_result(
                    format!("completed operation {operation_id} (revision {revision})"),
                    &snapshot,
                    workspace,
                )
            }
            DaemonReply::Ok(value) => {
                let snapshot = lifecycle_snapshot(&value)?;
                if snapshot.revision < self.last_revision {
                    return Ok(SessionCommandResult::message(
                        "ignored stale daemon snapshot",
                    ));
                }
                self.last_revision = snapshot.revision;
                session_snapshot_result("daemon snapshot refreshed", &snapshot, workspace)
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
    snapshot: &LifecycleSnapshot,
    workspace: &Workspace,
) -> Result<SessionCommandResult, String> {
    let session_ids = snapshot
        .available_sessions()
        .map(|session| session.session_id)
        .collect();
    let legacy = load_workspace_state(&workspace.path).map_err(|error| error.to_string())?;
    Ok(SessionCommandResult {
        message: message.into(),
        sessions: Some(snapshot.project(workspace, &legacy.sessions)),
        session_ids: Some(session_ids),
    })
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
    // Ctrl-A / Ctrl-E become semantic caret keys. A focused text field reads
    // them as emacs line-start / line-end; the reducer's navigation branch maps
    // `LineStart` back to the reserved `+ new session` action (IME-safe #287),
    // and `key_to_terminal_bytes` still forwards U+0001 / U+0005 to a focused
    // shell. `Home` / `End` carry the same split without the control modifier.
    if (key.modifiers.control && key.code == KeyCode::Char('a'))
        || key.code == KeyCode::Char('\u{1}')
    {
        return Key::LineStart;
    }
    if (key.modifiers.control && key.code == KeyCode::Char('e'))
        || key.code == KeyCode::Char('\u{5}')
    {
        return Key::LineEnd;
    }
    // Shift+motion extends a selection in the focused input; a live shell still
    // receives movement via `key_to_terminal_bytes`. Handle these before the
    // generic modified-chord passthrough below swallows the Shift.
    match key.code {
        KeyCode::Left if key.modifiers.shift => return Key::SelectLeft,
        KeyCode::Right if key.modifiers.shift => return Key::SelectRight,
        KeyCode::Home if key.modifiers.shift => return Key::SelectHome,
        KeyCode::End if key.modifiers.shift => return Key::SelectEnd,
        _ => {}
    }
    // The live classifier has already encoded the original terminal input.
    // Keep modified chords opaque so this management-key adapter cannot drop
    // their Ctrl/Alt bytes before Closeup forwards them to the focused pane.
    // Crossterm reports Shift even though `Char` already carries the resulting
    // uppercase (or shifted-symbol) Unicode scalar.  It is text input, not an
    // opaque terminal chord, so pass it to management forms normally.
    let shift_only = key.modifiers.shift
        && !key.modifiers.control
        && !key.modifiers.alt
        && !key.modifiers.super_
        && !key.modifiers.hyper
        && !key.modifiers.meta;
    if key.modifiers != Modifiers::default()
        && !(shift_only && matches!(key.code, KeyCode::Char(_)))
    {
        return Key::Passthrough(bytes);
    }
    match key.code {
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::Delete => Key::Delete,
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

/// Classify what currently exists at `path` for New-project pre-validation.
/// Metadata is resolved through symlinks; anything unreadable (including a
/// missing path or a broken link) is treated as [`WorkspaceProbe::Missing`],
/// and the subsequent clone/register would surface any deeper IO failure.
#[coverage(off)]
fn probe_path(path: &Path) -> workspace_usecase::WorkspaceProbe {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => workspace_usecase::WorkspaceProbe::Directory,
        Ok(_) => workspace_usecase::WorkspaceProbe::NonDirectory,
        Err(_) => workspace_usecase::WorkspaceProbe::Missing,
    }
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
            .available_sessions()
            .map(|session| session.session_id)
            .collect();
        state.sessions = lifecycle.project(&workspace, &state.sessions);
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

    #[coverage(off)]
    fn create_workspace(&mut self, request: &NewRequest) -> std::io::Result<WorkspaceSnapshot> {
        // 副作用（create_dir_all / git clone / registry 書き込み）の前に事前検証する。
        // 既存 workspace・不正パスはここで安全な 1 行メッセージにして返し、何も作らないまま
        // 呼び出し側（NewStep::Create 失敗枝）が draft を保って同画面で再試行できるようにする。
        let (kind, target): (workspace_usecase::NewWorkspaceKind, &Path) = match request {
            NewRequest::Clone { destination, .. } => {
                (workspace_usecase::NewWorkspaceKind::Clone, destination)
            }
            NewRequest::Existing { path, .. } => {
                (workspace_usecase::NewWorkspaceKind::Existing, path)
            }
        };
        let registered = workspace_usecase::is_registered(
            &self.storage.load_workspaces().map_err(io_error)?,
            target,
        );
        workspace_usecase::preflight_new_workspace(kind, registered, probe_path(target))
            .map_err(|error| io_error(error.message()))?;

        let path = match request {
            NewRequest::Clone {
                repository,
                destination,
                branch,
            } => {
                let parent = destination
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                let directory = destination
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| io_error("clone destination is not a valid directory name"))?;
                std::fs::create_dir_all(parent)?;
                git_clone(&SystemGit, parent, repository, directory, branch.as_deref())
                    .map_err(io_error)?
            }
            NewRequest::Existing { path, name } => {
                workspace_usecase::register(&self.storage, path, name, Utc::now())
                    .map_err(io_error)?;
                path.clone()
            }
        };
        // Clone / Existing どちらも、作成後は他の workspace と同じ open 経路で snapshot を得る。
        self.open(&path)
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

/// Composition adapter for the daemon-owned PR snapshot. It deliberately has no
/// local scanner or state fallback: a failed request remains a safe TUI message
/// and a later snapshot retries convergence.
struct DaemonPrSnapshotPort;

impl PrSnapshotPort for DaemonPrSnapshotPort {
    #[coverage(off)]
    fn snapshot(
        &mut self,
        session_id: usagi_core::domain::id::SessionId,
    ) -> Result<usagi_core::usecase::client::PrSnapshot, String> {
        let mut client = crate::runtime::daemon::client(ClientPolicy::tui())
            .map_err(|_| "daemon unavailable".to_owned())?;
        let reply = client
            .request(DaemonRequest::Pr {
                action: PrAction::Snapshot,
                payload: PrRequest {
                    session_id,
                    revision: None,
                },
            })
            .map_err(|_| "daemon unavailable".to_owned())?;
        match reply {
            DaemonReply::Ok(value) => usagi_core::usecase::client::decode_pr_snapshot(value)
                .map_err(|_| "invalid PR snapshot".to_owned()),
            DaemonReply::Accepted { .. } => Err("PR snapshot is unavailable".to_owned()),
        }
    }
}

/// OS adapter for the browser effect. `Command` receives separate argv items; no
/// URL is ever interpolated into a shell command.
struct PlatformBrowserOpener;

impl BrowserOpener for PlatformBrowserOpener {
    #[coverage(off)]
    fn open(&mut self, url: &str) -> Result<(), String> {
        let mut command = if cfg!(target_os = "macos") {
            let mut command = Command::new("open");
            command.arg(url);
            command
        } else if cfg!(target_os = "linux") {
            let mut command = Command::new("xdg-open");
            command.arg(url);
            command
        } else if cfg!(target_os = "windows") {
            // `start` is a `cmd` builtin, so it is launched through `cmd /C`. Its
            // first quoted argument is the (empty) window title `start` consumes,
            // so a URL beginning with `"` is never mistaken for the title. The URL
            // stays a distinct argv item — `cmd` does not re-parse it as a command.
            let mut command = Command::new("cmd");
            command.args(["/C", "start", "", url]);
            command
        } else {
            return Err("browser opening is unsupported on this platform".to_owned());
        };
        command
            .spawn()
            .map(|_| ())
            .map_err(|_| "browser launch failed".to_owned())
    }
}

#[coverage(off)]
fn launch_workspace(out: &mut dyn Write, path: &Path) -> std::io::Result<()> {
    let mut loader = FsWorkspaceLoader::open_default()?;
    let snapshot = loader.open(path)?;
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        run_with_metrics_hook(|| {
            run_in_terminal(|terminal| {
                presentation::run_workspace_controller(
                    terminal,
                    snapshot,
                    Box::new(DaemonSessionCommandPort::default()),
                    Box::new(DaemonAgentCommandPort::new()),
                    Box::new(DaemonDecisionCommandPort),
                    Box::new(PlatformDesktopNotifier),
                    Box::new(DaemonMetricsPort::new()),
                    Box::new(DaemonPrSnapshotPort),
                    Box::new(PlatformBrowserOpener),
                )
            })
        })?;
    } else {
        for line in presentation::render_home_snapshot(0, 0, &snapshot) {
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
    if !matches!(entry, EntryScreen::Config | EntryScreen::Doctor)
        && let Err(error) = crate::runtime::daemon::ensure_ready()
    {
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
        LifecycleSnapshot, PersistentSettingsPort, Start, TerminalChunk, TerminalError,
        control_key, created_session_hook, decode_terminal_poll, load_screen_graph_data,
        load_workspace_state, passthrough_key,
    };
    use chrono::Utc;
    use serde_json::json;
    use usagi_core::domain::id::{OperationId, WorkspaceId};
    use usagi_core::domain::note::Scratchpad;
    use usagi_core::domain::session::{SessionOrigin, SessionRecord};
    use usagi_core::domain::session_lifecycle::{ManagedSession, SessionLifecycle};
    use usagi_core::domain::settings::{ModalSelectionMode, Settings};
    use usagi_core::infrastructure::paths::project_data_dir;
    use usagi_core::infrastructure::store::workspace::Storage;
    use usagi_core::usecase::settings::{SettingsPort, SettingsScope};
    use usagi_tui::usecase::application::Key;
    use usagi_tui::usecase::terminal_input::{
        KeyCode, KeyEvent, KeyEventKind, LiveInput, Modifiers,
    };

    /// A pressed [`LiveInput::Key`] with the given code and modifiers.
    fn live_key(code: KeyCode, modifiers: Modifiers) -> LiveInput {
        LiveInput::Key(KeyEvent::new(code, modifiers, KeyEventKind::Press))
    }

    /// The Control-only modifier set.
    fn control() -> Modifiers {
        Modifiers {
            control: true,
            ..Modifiers::default()
        }
    }

    #[test]
    fn decode_terminal_poll_returns_output_chunks_while_running() {
        let body = json!({
            "output": [{"start_offset": 0, "end_offset": 3, "data": b"abc".to_vec()}],
            "exited": false,
        });
        assert_eq!(
            decode_terminal_poll(&body),
            Ok(vec![TerminalChunk {
                start_offset: 0,
                end_offset: 3,
                data: b"abc".to_vec(),
            }])
        );
    }

    #[test]
    fn decode_terminal_poll_treats_a_missing_exited_flag_as_running() {
        // A daemon reply that omits `exited` (or predates the field) must not be
        // read as an exit, so a live pane tab is never dropped spuriously.
        let body = json!({ "output": [] });
        assert_eq!(decode_terminal_poll(&body), Ok(Vec::new()));
    }

    #[test]
    fn decode_terminal_poll_surfaces_exit_once_output_is_drained() {
        let body = json!({ "output": [], "exited": true });
        assert_eq!(decode_terminal_poll(&body), Err(TerminalError::Exited));
    }

    #[test]
    fn decode_terminal_poll_yields_final_output_before_reporting_exit() {
        // The exit reply may still carry fresh output; it is applied first and the
        // exit is reported on the next (drained) poll, preserving final output.
        let body = json!({
            "output": [{"start_offset": 6, "end_offset": 8, "data": b"hi".to_vec()}],
            "exited": true,
        });
        assert_eq!(
            decode_terminal_poll(&body),
            Ok(vec![TerminalChunk {
                start_offset: 6,
                end_offset: 8,
                data: b"hi".to_vec(),
            }])
        );
    }

    #[test]
    fn decode_terminal_poll_rejects_a_malformed_output_frame() {
        let body = json!({ "output": [{"end_offset": 3, "data": b"abc".to_vec()}] });
        assert_eq!(decode_terminal_poll(&body), Err(TerminalError::Unavailable));
    }

    #[test]
    fn lifecycle_snapshot_excludes_failed_sessions_from_the_tui_projection() {
        let mut available =
            ManagedSession::new_creating("available".into(), OperationId::new(), Utc::now());
        available.lifecycle = SessionLifecycle::Available;
        let mut failed =
            ManagedSession::new_creating("failed".into(), OperationId::new(), Utc::now());
        failed.lifecycle = SessionLifecycle::Failed;
        let snapshot = LifecycleSnapshot {
            workspace_id: WorkspaceId::new(),
            root_worktree_id: usagi_core::domain::id::WorktreeId::new(),
            revision: 1,
            sessions: vec![available, failed],
        };

        assert_eq!(
            snapshot
                .available_sessions()
                .map(|session| session.name.as_str())
                .collect::<Vec<_>>(),
            ["available"]
        );
    }

    #[test]
    fn daemon_restart_projection_retains_legacy_ui_metadata() {
        let temporary = tempfile::tempdir().unwrap();
        let workspace = usagi_core::domain::workspace::Workspace::new("repo", temporary.path());
        let mut available =
            ManagedSession::new_creating("legacy".into(), OperationId::new(), Utc::now());
        available.lifecycle = SessionLifecycle::Available;
        let snapshot = LifecycleSnapshot {
            workspace_id: WorkspaceId::new(),
            root_worktree_id: usagi_core::domain::id::WorktreeId::new(),
            revision: 2,
            sessions: vec![available],
        };
        let legacy = SessionRecord {
            name: "legacy".into(),
            display_name: Some("Keep me".into()),
            origin: SessionOrigin::Mcp,
            started_from: Some("parent".into()),
            root: temporary.path().join("stale-root"),
            created_at: Utc::now(),
            last_active: Some(Utc::now()),
            notes: Scratchpad {
                note: Some("do not drop".into()),
                ..Default::default()
            },
            prs: Vec::new(),
        };

        let projected = snapshot.project(&workspace, &[legacy]);
        assert_eq!(projected[0].display_name.as_deref(), Some("Keep me"));
        assert_eq!(projected[0].origin, SessionOrigin::Mcp);
        assert_eq!(projected[0].notes.note.as_deref(), Some("do not drop"));
        assert_eq!(
            projected[0].root,
            temporary.path().join(".usagi/sessions/legacy")
        );
    }

    #[test]
    fn ctrl_a_and_ctrl_e_map_to_semantic_line_edge_keys() {
        // Ctrl-A → LineStart (emacs line-start in a text field; `+ new session`
        // in navigation, resolved downstream). Both the modified `a` and the raw
        // U+0001 decoding reach the same key.
        let ctrl_a = live_key(KeyCode::Char('a'), control());
        assert_eq!(passthrough_key(&ctrl_a, Vec::new()), Key::LineStart);
        let raw_soh = live_key(KeyCode::Char('\u{1}'), Modifiers::default());
        assert_eq!(passthrough_key(&raw_soh, Vec::new()), Key::LineStart);

        // Ctrl-E → LineEnd, from both the modified `e` and raw U+0005.
        let ctrl_e = live_key(KeyCode::Char('e'), control());
        assert_eq!(passthrough_key(&ctrl_e, Vec::new()), Key::LineEnd);
        let raw_enq = live_key(KeyCode::Char('\u{5}'), Modifiers::default());
        assert_eq!(passthrough_key(&raw_enq, Vec::new()), Key::LineEnd);
    }

    #[test]
    fn plain_home_end_and_delete_reach_the_input_as_caret_keys() {
        assert_eq!(
            passthrough_key(&live_key(KeyCode::Home, Modifiers::default()), Vec::new()),
            Key::Home
        );
        assert_eq!(
            passthrough_key(&live_key(KeyCode::End, Modifiers::default()), Vec::new()),
            Key::End
        );
        assert_eq!(
            passthrough_key(&live_key(KeyCode::Delete, Modifiers::default()), Vec::new()),
            Key::Delete
        );
    }

    #[test]
    fn shift_motion_extends_a_selection_without_being_swallowed_as_a_chord() {
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        assert_eq!(
            passthrough_key(&live_key(KeyCode::Left, shift), b"\x1b[1;2D".to_vec()),
            Key::SelectLeft
        );
        assert_eq!(
            passthrough_key(&live_key(KeyCode::Right, shift), b"\x1b[1;2C".to_vec()),
            Key::SelectRight
        );
        assert_eq!(
            passthrough_key(&live_key(KeyCode::Home, shift), b"\x1b[1;2H".to_vec()),
            Key::SelectHome
        );
        assert_eq!(
            passthrough_key(&live_key(KeyCode::End, shift), b"\x1b[1;2F".to_vec()),
            Key::SelectEnd
        );
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
    fn shifted_characters_reach_management_text_inputs() {
        let shifted_uppercase = LiveInput::Key(KeyEvent::new(
            KeyCode::Char('A'),
            Modifiers {
                shift: true,
                ..Modifiers::default()
            },
            KeyEventKind::Press,
        ));
        assert_eq!(
            passthrough_key(&shifted_uppercase, b"A".to_vec()),
            Key::Char('A')
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
        let state_dir = project_data_dir(workspace.path());
        std::fs::create_dir_all(&state_dir).unwrap();
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

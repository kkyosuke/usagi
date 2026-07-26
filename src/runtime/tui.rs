//! TUI 面へ実端末と filesystem を接続する composition adapter。

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::{sync::mpsc, thread};

use chrono::Utc;
use crossterm::cursor;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::terminal::{
    self, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{execute, queue};
use usagi_core::domain::AppInfo;
use usagi_core::domain::agent::{ProviderResumeProjection, ProviderResumeReason};
use usagi_core::domain::id::{SessionId, UserDecisionId, WorkspaceId};
use usagi_core::domain::note::Scratchpad;
use usagi_core::domain::recent::Recent;
use usagi_core::domain::session::{SessionOrigin, SessionRecord};
use usagi_core::domain::session_lifecycle::{ManagedSession, SessionLifecycleProjection};
use usagi_core::domain::settings::{
    EnvBindings, LocalSettings, Settings, format_env_bindings, parse_env_bindings,
};
use usagi_core::domain::terminal_launch::{
    TerminalLaunchRequest, TerminalLaunchScope, TerminalProfileId,
};
use usagi_core::domain::user_decision::UserDecisionAnswer;
use usagi_core::domain::workspace::Workspace;
use usagi_core::infrastructure::error_log::ErrorLog;
use usagi_core::infrastructure::git::{clone as git_clone, diff_status};
use usagi_core::infrastructure::ipc::{TerminalInputReplayMode, TerminalSnapshotMode};
use usagi_core::infrastructure::store::settings::WorkspaceSettingsStore;
use usagi_core::infrastructure::store::state::WorkspaceStateStore;
use usagi_core::infrastructure::store::workspace::Storage;
use usagi_core::usecase::client::{
    AgentLaunchIntent, ClientError, ClientPolicy, DaemonClient, DaemonMetrics, DaemonReply,
    DaemonRequest, MetricsAction, PrAction, PrRequest, SessionAction, TerminalAction,
    TerminalGeometry, TerminalLaneBudget, TerminalLaunchIntent, TerminalRequest,
};
use usagi_core::usecase::env::EnvScope;
use usagi_core::usecase::note::Target as StoreTarget;
use usagi_core::usecase::settings::{SettingsPort, SettingsScope};
use usagi_core::usecase::vt_screen::ScreenCheckpoint;
use usagi_core::usecase::workspace as workspace_usecase;
use usagi_daemon::usecase::session_runtime::SystemGit;
use usagi_tui::infrastructure::metrics::MetricsHook;
use usagi_tui::presentation::frame::{Frame, FrameRenderer};
use usagi_tui::presentation::views::config::{self, AvailableAgentModels, Config};
use usagi_tui::presentation::views::pr_modal::PrModal;
use usagi_tui::presentation::views::welcome::{self, Welcome};
use usagi_tui::presentation::views::workspace::GitDiff;
use usagi_tui::presentation::{
    self, AgentCommandPort, AgentPaneAdmission, BannerScreenRunner, ControllerBackendComposition,
    ControllerBackendFactory, ControllerHost, DecisionCommandPort, DesktopNotificationPort,
    EnvironmentStorePort, ExactAgentResume, Exit, ExternalTerminalPort, MetricsPort,
    RestoreConnectionPort, SerializedPaneLaunchPort, SessionCommandPort, SessionCommandResult,
    SessionRefreshPort, Start, WorkspaceLoader, WorkspaceSnapshot,
};
use usagi_tui::usecase::application::agent_tab_intent::{
    AgentTabIntent, AgentTabIntentError, AgentTabIntentMutation, AgentTabIntentPort,
    AgentTabIntentPortCommit,
};
use usagi_tui::usecase::application::controller::{
    BackendEvent, EnvironmentEntry, NewRequest, Notice, SafeError, SafeMessage, Target,
};
use usagi_tui::usecase::application::daemon_backend::{
    Completions, DaemonBackend, DecisionPort as BackendDecisionPort,
    OverlayPort as BackendOverlayPort, TargetStorePort as BackendTargetStorePort,
    WorkspaceCommandPort as BackendWorkspaceCommandPort,
};
use usagi_tui::usecase::application::pane_runtime::Geometry;
use usagi_tui::usecase::application::pr::{BrowserOpener, PrSnapshotPort};
use usagi_tui::usecase::application::terminal_session::{
    TerminalAttach, TerminalAttachScreen, TerminalChunk, TerminalError, TerminalInputOutcome,
    TerminalInputResolution, TerminalSubscription,
};
use usagi_tui::usecase::application::{self, EntryScreen, Key, Terminal};
use usagi_tui::usecase::overview;
use usagi_tui::usecase::overview::SessionCommand;
use usagi_tui::usecase::terminal_input::{
    GlobalControlChord, KeyCode, KeyEventKind, LiveInput, LiveInputClassifier, LiveInputOutput,
    Modifiers, RuntimeEvent,
};

use crate::runtime::agent_tab_intent::FileAgentTabIntentStore;
use crate::runtime::clipboard::PlatformClipboard;
use crate::runtime::daemon::LaneClient;
use crate::runtime::inventory_pump::TerminalInventoryPump;
use crate::runtime::refresh_pump::{RefreshCadence, RefreshPump};
use crate::runtime::terminal_pump::TerminalPollPump;
use crate::tui_input::{CrosstermSource, EventPump, NoBackend};

/// Composition adapter for Overview's daemon-owned session lifecycle commands.
#[derive(Default)]
struct DaemonSessionCommandPort;

/// Production bridge for the controller's durable user-decision effects.
/// The daemon remains the authority; this adapter only converts its safe
/// snapshots and confirmations back into reducer events.
struct DaemonDecisionCommandPort;

/// Platform delivery is deliberately best-effort.  Fixed executable names and
/// argument-vector spawning keep decision text out of a shell; a missing
/// notification service must never stop the TUI.
struct PlatformDesktopNotifier;

#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=production_backend_factory_effect_matrix
impl DesktopNotificationPort for PlatformDesktopNotifier {
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
    #[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=decision_port_completion_contract
    fn client() -> Result<impl DaemonClient, String> {
        match crate::runtime::daemon::policy_client(ClientPolicy::tui()) {
            Ok(client) => Ok(client),
            Err(error) => Err(format!("daemon unavailable: {error}")),
        }
    }

    fn safe_error(error: impl std::fmt::Display) -> SafeError {
        SafeError {
            message: SafeMessage::new(error.to_string()),
            error_id: "decision-daemon-error".to_owned(),
        }
    }
}

#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=decision_port_completion_contract
impl DecisionCommandPort for DaemonDecisionCommandPort {
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

/// Production store for the controller's notes and environment effects.
///
/// Notes stay in the repository's `state.json` (the same store todos/decisions
/// use), keyed by the controller's stable [`Target`] identity. Environment
/// bindings are configuration rather than session state, so they live in the two
/// settings files and are reached through [`SettingsEnvironmentStore`]. Both
/// project each read/write back as a controller [`BackendEvent`].
struct RepoEnvironmentStore {
    store: WorkspaceStateStore,
    /// Stable session identities paired with their store names, captured from
    /// the snapshot the runtime opened with (the TUI never infers a name from an
    /// id elsewhere).
    session_names: Vec<(usagi_core::domain::id::SessionId, String)>,
    environment: SettingsEnvironmentStore,
}

impl RepoEnvironmentStore {
    fn new(
        workspace_path: &Path,
        session_names: Vec<(usagi_core::domain::id::SessionId, String)>,
        environment: SettingsEnvironmentStore,
    ) -> Self {
        Self {
            store: WorkspaceStateStore::new(workspace_path),
            session_names,
            environment,
        }
    }

    /// Resolve a controller target to the name-keyed store target, or `None`
    /// when a session id is no longer in the snapshot (a stale target).
    fn resolve(&self, target: Target) -> Option<StoreTarget<'_>> {
        match target {
            Target::Root(_) => Some(StoreTarget::Root),
            Target::Session(id) => self
                .session_names
                .iter()
                .find(|(known, _)| *known == id)
                .map(|(_, name)| StoreTarget::Session(name.as_str())),
        }
    }

    fn safe_error(reason: impl std::fmt::Display) -> SafeError {
        SafeError {
            message: SafeMessage::new(reason.to_string()),
            error_id: "target-store-error".to_owned(),
        }
    }

    fn stale_target() -> SafeError {
        Self::safe_error("this session is no longer available")
    }
}

fn environment_entries(map: BTreeMap<String, String>) -> Vec<EnvironmentEntry> {
    map.into_iter()
        .map(|(name, value)| EnvironmentEntry { name, value })
        .collect()
}

fn environment_map(entries: &[EnvironmentEntry]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|entry| (entry.name.clone(), entry.value.clone()))
        .collect()
}

/// The two settings files that own environment bindings: the per-user
/// `settings.json` in the data directory (every workspace inherits it) and the
/// workspace's own `<workspace>/.usagi/settings.json`.
///
/// A read reports the edited scope's own bindings plus what it inherits, so the
/// workspace editor can show the global set without ever copying it into the
/// workspace file. A write replaces exactly one scope's bindings under that
/// scope's cross-process lock, leaving the rest of the settings file untouched.
struct SettingsEnvironmentStore {
    global: Storage,
    workspace: WorkspaceSettingsStore,
}

impl SettingsEnvironmentStore {
    fn new(data_dir: PathBuf, workspace_root: &Path) -> Self {
        Self {
            global: Storage::new(data_dir),
            workspace: WorkspaceSettingsStore::new(workspace_root),
        }
    }

    /// `(the scope's own bindings, the bindings it inherits)`.
    fn read(&self, scope: EnvScope) -> anyhow::Result<(EnvBindings, EnvBindings)> {
        let global = self.global.load_settings()?.env;
        Ok(match scope {
            // Global inherits nothing: it *is* what the others inherit.
            EnvScope::Global => (global, EnvBindings::new()),
            EnvScope::Workspace => (self.workspace.load()?.env, global),
        })
    }

    fn write(&self, scope: EnvScope, bindings: EnvBindings) -> anyhow::Result<()> {
        match scope {
            EnvScope::Global => {
                let _lock = self.global.lock()?;
                let mut settings = self.global.load_settings()?;
                settings.env = bindings;
                self.global.save_settings(&settings)?;
            }
            EnvScope::Workspace => {
                let _lock = self.workspace.lock()?;
                let mut local = self.workspace.load()?;
                local.env = bindings;
                self.workspace.save(&local)?;
            }
        }
        Ok(())
    }

    fn safe_error(reason: impl std::fmt::Display) -> SafeError {
        SafeError {
            message: SafeMessage::new(reason.to_string()),
            error_id: "environment-settings-error".to_owned(),
        }
    }
}

#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=settings_environment_store_persistence_contract
impl EnvironmentStorePort for SettingsEnvironmentStore {
    fn load(&mut self, scope: EnvScope) -> BackendEvent {
        match self.read(scope) {
            Ok((entries, inherited)) => BackendEvent::EnvironmentLoaded {
                scope,
                entries: environment_entries(entries),
                inherited: environment_entries(inherited),
            },
            Err(error) => BackendEvent::EnvironmentError {
                scope,
                error: Self::safe_error(error),
            },
        }
    }

    fn save(&mut self, scope: EnvScope, entries: Vec<EnvironmentEntry>) -> BackendEvent {
        // Save through the same validation a launch applies, then reflux what
        // actually landed so the editor mirrors the stored file.
        let bindings = parse_env_bindings(&format_env_bindings(&environment_map(&entries)));
        match self.write(scope, bindings) {
            Ok(()) => EnvironmentStorePort::load(self, scope),
            Err(error) => BackendEvent::EnvironmentError {
                scope,
                error: Self::safe_error(error),
            },
        }
    }
}

#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=repo_environment_store_persistence_contract
impl BackendTargetStorePort for RepoEnvironmentStore {
    fn load_notes(&mut self, target: Target, completions: Completions) {
        let event = match self.resolve(target) {
            Some(scope) => match usagi_core::usecase::note::note(&self.store, scope) {
                Ok(note) => BackendEvent::NotesLoaded {
                    target,
                    scratchpad: Scratchpad {
                        note,
                        ..Scratchpad::default()
                    },
                },
                Err(error) => BackendEvent::NotesError {
                    target,
                    error: Self::safe_error(error),
                },
            },
            None => BackendEvent::NotesError {
                target,
                error: Self::stale_target(),
            },
        };
        completions.emit(usagi_tui::usecase::application::controller::AppEvent::Backend(event));
    }

    fn save_notes(&mut self, target: Target, scratchpad: Scratchpad, completions: Completions) {
        let event = (|| -> Result<BackendEvent, SafeError> {
            let session_name = match target {
                Target::Root(_) => None,
                Target::Session(id) => Some(
                    self.session_names
                        .iter()
                        .find(|(known, _)| *known == id)
                        .map(|(_, name)| name.clone())
                        .ok_or_else(Self::stale_target)?,
                ),
            };
            let _lock = self.store.lock().map_err(Self::safe_error)?;
            let mut state = self
                .store
                .load()
                .map_err(Self::safe_error)?
                .unwrap_or_default();
            match session_name {
                None => state.root_notes = scratchpad.clone(),
                Some(name) => {
                    let record = state
                        .sessions
                        .iter_mut()
                        .find(|record| record.name == name)
                        .ok_or_else(Self::stale_target)?;
                    record.notes = scratchpad.clone();
                }
            }
            state.updated_at = Utc::now();
            self.store.save(&state).map_err(Self::safe_error)?;
            Ok(BackendEvent::NotesLoaded { target, scratchpad })
        })()
        .unwrap_or_else(|error| BackendEvent::NotesError { target, error });
        completions.emit(usagi_tui::usecase::application::controller::AppEvent::Backend(event));
    }

    fn load_environment(&mut self, scope: EnvScope, completions: Completions) {
        completions.emit(
            usagi_tui::usecase::application::controller::AppEvent::Backend(
                EnvironmentStorePort::load(&mut self.environment, scope),
            ),
        );
    }

    fn save_environment(
        &mut self,
        scope: EnvScope,
        entries: Vec<EnvironmentEntry>,
        completions: Completions,
    ) {
        completions.emit(
            usagi_tui::usecase::application::controller::AppEvent::Backend(
                EnvironmentStorePort::save(&mut self.environment, scope, entries),
            ),
        );
    }
}

/// Home's durable-decision lane.
///
/// The snapshot used to be fetched inline: every terminal wake-up dispatched
/// `Effect::RefreshDecisions`, and this port ran the whole `bootstrap_client` →
/// request → close round trip on the render thread at the 16ms frame tick
/// (#551). It now owns a [`RefreshPump`] instead: a resident worker observes on
/// its own persistent connection at a bounded cadence and this port only hands
/// the newest snapshot over, so `refresh` is a wake and `poll` is a drain.
///
/// Desktop notification stays here rather than in the worker: the dedup set is
/// render-thread state, and `notify` only spawns a detached process.
struct ProductionDecisionPort {
    daemon: DaemonDecisionCommandPort,
    notifier: PlatformDesktopNotifier,
    notified: std::collections::BTreeSet<UserDecisionId>,
    /// The workspace every observation of this lane belongs to, fixed when the
    /// composition opened it.
    workspace: WorkspaceId,
    pump: RefreshPump<Vec<usagi_core::domain::user_decision::UserDecision>>,
    /// Whether the current failure streak has already been reported. A lane that
    /// observes twice a second must notice a missing daemon **once**, not twice
    /// a second, so the notice is tied to entering the failure state rather than
    /// to each failed round.
    reported_failure: bool,
}

impl ProductionDecisionPort {
    /// Turn one completed observation into the reducer's event vocabulary,
    /// notifying the desktop about decisions this run has not announced yet.
    #[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=refresh_pump_lane_contract
    fn publish(
        &mut self,
        result: Result<Vec<usagi_core::domain::user_decision::UserDecision>, String>,
        completions: &Completions,
    ) {
        let event = match result {
            Ok(decisions) => {
                self.reported_failure = false;
                for decision in &decisions {
                    if self.notified.insert(decision.decision_id) {
                        self.notifier
                            .notify("usagi: decision needed", &decision.title);
                    }
                }
                BackendEvent::Decisions {
                    workspace: self.workspace,
                    decisions,
                }
            }
            Err(_) if self.reported_failure => return,
            Err(error) => {
                self.reported_failure = true;
                BackendEvent::Notice(Notice::new(error))
            }
        };
        completions.emit(usagi_tui::usecase::application::controller::AppEvent::Backend(event));
    }
}

#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=production_backend_factory_effect_matrix
impl BackendDecisionPort for ProductionDecisionPort {
    fn poll(&mut self, completions: &Completions) {
        if let Some(result) = self.pump.take() {
            self.publish(result, completions);
        }
    }

    fn refresh(&mut self, _workspace: WorkspaceId, completions: Completions) {
        // An explicit refresh is an out-of-cadence wake, never a request on this
        // thread — and it is also what starts the lane, so a composition nobody
        // drives never reaches the daemon. Whatever the lane has already observed
        // is handed over now; the wake's own result arrives through `poll` on a
        // later frame.
        self.pump.wake();
        if let Some(result) = self.pump.take() {
            self.publish(result, &completions);
        }
    }

    fn resolve(
        &mut self,
        workspace: WorkspaceId,
        decision_id: UserDecisionId,
        answer: UserDecisionAnswer,
        completions: Completions,
    ) {
        completions.emit(
            usagi_tui::usecase::application::controller::AppEvent::Backend(self.daemon.resolve(
                workspace,
                decision_id,
                answer,
            )),
        );
    }
}

struct ProductionOverlayPort {
    workspace_name: String,
    root: PathBuf,
    sessions: Vec<(usagi_core::domain::id::SessionId, String, PathBuf)>,
    prs: DaemonPrSnapshotPort,
    browser: PlatformBrowserOpener,
}

#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=production_backend_factory_effect_matrix
impl BackendOverlayPort for ProductionOverlayPort {
    fn load_pull_requests(&mut self, target: Target, completions: Completions) {
        let event = match target {
            Target::Root(_) => BackendEvent::PullRequestsLoaded {
                target,
                prs: Vec::new(),
            },
            Target::Session(session) => match self.prs.snapshot(session) {
                Ok(snapshot) => BackendEvent::PullRequestsLoaded {
                    target,
                    prs: PrModal::from_entries(&snapshot.entries).prs().to_vec(),
                },
                Err(message) => BackendEvent::PullRequestsError {
                    target,
                    error: SafeError {
                        message: SafeMessage::new(message),
                        error_id: "pr-load".to_owned(),
                    },
                },
            },
        };
        completions.emit(usagi_tui::usecase::application::controller::AppEvent::Backend(event));
    }

    fn load_preview(&mut self, target: Target, completions: Completions) {
        let lines = match target {
            Target::Root(_) => vec![
                format!("workspace: {}", self.workspace_name),
                format!("path: {}", self.root.display()),
            ],
            Target::Session(id) => self
                .sessions
                .iter()
                .find(|(known, _, _)| *known == id)
                .map_or_else(
                    || vec!["session is no longer available".to_owned()],
                    |(_, name, path)| {
                        vec![
                            format!("session: {name}"),
                            format!("path: {}", path.display()),
                        ]
                    },
                ),
        };
        completions.emit(
            usagi_tui::usecase::application::controller::AppEvent::Backend(
                BackendEvent::PreviewLoaded { target, lines },
            ),
        );
    }

    fn open_pull_request(&mut self, url: String, completions: Completions) {
        let event = match usagi_tui::usecase::application::pr::canonical_browser_url(&url) {
            Some(url) => self.browser.open(&url).err().map(|message| {
                BackendEvent::Notice(Notice::new(format!("Could not open browser: {message}")))
            }),
            None => Some(BackendEvent::Notice(Notice::new(
                "Cannot open an invalid PR URL.",
            ))),
        };
        if let Some(event) = event {
            completions.emit(usagi_tui::usecase::application::controller::AppEvent::Backend(event));
        }
    }
}

struct ProductionWorkspaceCommands;

#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=production_backend_factory_effect_matrix
impl BackendWorkspaceCommandPort for ProductionWorkspaceCommands {
    fn execute(&mut self, _: WorkspaceId, command: overview::Command, completions: Completions) {
        completions.emit(
            usagi_tui::usecase::application::controller::AppEvent::Backend(BackendEvent::Notice(
                Notice::new(format!("{} is not available", command.name())),
            )),
        );
    }
}

struct ProductionBackendFactory;

type EnvironmentSessionNames = Vec<(usagi_core::domain::id::SessionId, String)>;
type OverlaySessions = Vec<(usagi_core::domain::id::SessionId, String, PathBuf)>;

#[coverage(off)] // coverage: reason=generic_monomorphization owner=tui expires=2027-01-31 tests=production_backend_factory_preserves_terminal_arguments_and_completes_store_routes
fn project_backend_sessions(
    snapshot: &WorkspaceSnapshot,
) -> (EnvironmentSessionNames, OverlaySessions) {
    let mut names = Vec::new();
    let mut overlays = Vec::new();
    for (id, record) in snapshot.session_ids.iter().zip(&snapshot.state.sessions) {
        names.push((*id, record.name.clone()));
        let label = record
            .display_name
            .clone()
            .unwrap_or_else(|| record.name.clone());
        overlays.push((*id, label, record.root.clone()));
    }
    (names, overlays)
}

impl ControllerBackendFactory for ProductionBackendFactory {
    fn create(
        &mut self,
        snapshot: &WorkspaceSnapshot,
        host: ControllerHost,
    ) -> ControllerBackendComposition {
        let (session_names, sessions) = project_backend_sessions(snapshot);
        let environment_data_dir = usagi_core::infrastructure::paths::data_dir()
            .expect("workspace launch already resolved the daemon data directory");
        let store = RepoEnvironmentStore::new(
            &snapshot.workspace.path,
            session_names,
            SettingsEnvironmentStore::new(environment_data_dir, &snapshot.workspace.path),
        );
        let backend = DaemonBackend::new(
            Box::new(host.clone()),
            Box::new(host),
            Box::new(store),
            Box::new(ProductionWorkspaceCommands),
        )
        .with_decisions(Box::new(ProductionDecisionPort {
            daemon: DaemonDecisionCommandPort,
            notifier: PlatformDesktopNotifier,
            notified: std::collections::BTreeSet::new(),
            workspace: snapshot.workspace_id,
            pump: spawn_decision_pump(),
            reported_failure: false,
        }))
        .with_overlay(Box::new(ProductionOverlayPort {
            workspace_name: snapshot.workspace.name.clone(),
            root: snapshot.workspace.path.clone(),
            sessions,
            prs: DaemonPrSnapshotPort,
            browser: PlatformBrowserOpener,
        }));
        let data_dir = usagi_core::infrastructure::paths::data_dir()
            .expect("workspace launch already resolved the daemon data directory");
        let (restore_connection, restore_publisher) =
            DaemonRestoreConnectionPort::channel(data_dir);
        ControllerBackendComposition {
            backend,
            session_commands: Box::new(DaemonSessionCommandPort),
            // The resident session-inventory lane. It is a separate client from
            // `session_commands` on purpose: a user-initiated create/remove and
            // the background observation must not queue behind each other.
            session_refresh: Box::new(DaemonSessionRefreshPort {
                pump: spawn_session_refresh_pump(snapshot.workspace.clone()),
            }),
            agent_commands: Box::new(
                DaemonAgentCommandPort::new(spawn_poll_pump())
                    .with_inventory_pump(spawn_inventory_pump()),
            ),
            // A third daemon client, dedicated to pane launches. Keeping it out
            // of the resident stream client is what lets a slow or hung launch
            // leave existing panes' poll / input / resize / detach untouched.
            // It attaches to nothing, so its poll pump stays idle and is joined
            // when the workspace's composition drops.
            pane_launch_commands: Box::new(SerializedPaneLaunchPort::new(Box::new(
                DaemonAgentCommandPort::new(spawn_poll_pump()),
            ))),
            restore_commands: Box::new(
                DaemonAgentCommandPort::new(spawn_poll_pump())
                    .with_restore_connection(restore_publisher),
            ),
            restore_connection: Box::new(restore_connection),
            agent_tab_intents: Box::new(UserAgentTabIntentPort::new()),
            external_terminal: Box::new(PlatformExternalTerminalPort),
            metrics: Box::new(DaemonMetricsPort::new()),
            browser: Box::new(PlatformBrowserOpener),
            // Off the frame budget: the inline create form is the only reader,
            // so the scan runs only while that form owns input (#554).
            session_worktrees: Box::new(presentation::FsSessionWorktreeScanPort),
        }
    }
}

/// Home's resident session-inventory lane.
///
/// It replaces the per-tick `SessionCommand::List` worker: that spawned one OS
/// thread and one bootstrapped daemon connection for every completed round, at
/// the frame tick's cadence (#551). This port owns neither policy nor IO — the
/// [`RefreshPump`] owns the cadence and the connection, and the frame loop only
/// wakes and drains.
struct DaemonSessionRefreshPort {
    pump: RefreshPump<SessionCommandResult>,
}

#[coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=refresh_pump_lane_contract
impl SessionRefreshPort for DaemonSessionRefreshPort {
    fn wake(&mut self) {
        self.pump.wake();
    }

    fn take(&mut self) -> Option<Result<SessionCommandResult, String>> {
        self.pump.take()
    }
}

/// The mascot's metrics and the sidebar's git columns.
///
/// `latest` used to sample the daemon inline behind a one-second cache, but that
/// one sample per second still opened a fresh bootstrapped connection **on the
/// render thread** (#551). The sampling rate is unchanged; the request now
/// belongs to a resident lane and `latest` is a cache read that cannot block.
/// Git diffs were already on a worker thread and keep that shape.
struct DaemonMetricsPort {
    metrics: RefreshPump<Option<DaemonMetrics>>,
    latest: Option<DaemonMetrics>,
    git_diffs: BTreeMap<usagi_core::domain::id::SessionId, GitDiff>,
    git_receiver: Option<mpsc::Receiver<(usagi_core::domain::id::SessionId, GitDiff)>>,
    last_git_refresh: Option<Instant>,
}

impl DaemonMetricsPort {
    // Composition-only adapter: it spawns the real daemon lane and uses the
    // monotonic clock. The presentation `MetricsPort` is covered with fakes.
    #[coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=production_metrics_projection_contract
    fn new() -> Self {
        Self {
            metrics: spawn_metrics_pump(),
            latest: None,
            git_diffs: BTreeMap::new(),
            git_receiver: None,
            last_git_refresh: None,
        }
    }
}
#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=production_metrics_projection_contract
impl MetricsPort for DaemonMetricsPort {
    // Real daemon I/O belongs to the composition root; UI behaviour is tested
    // through its injected MetricsPort boundary.
    fn latest(&mut self) -> Option<DaemonMetrics> {
        // The first frame that wants a sample starts the lane; a composition
        // nobody draws never reaches the daemon.
        self.metrics.activate();
        // A failed observation keeps the previous sample rather than blanking
        // the mascot: the lane's backoff already reports the outage rate, and a
        // momentary daemon hiccup should not flicker the frame.
        if let Some(Ok(sample)) = self.metrics.take() {
            self.latest = sample;
        }
        self.latest.clone()
    }

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

/// Root composition adapter for the only Agent launch authority: the daemon.
///
/// Terminal streaming keeps one persistent daemon connection for its lifetime:
/// the daemon fences a terminal subscription (and therefore input/detach) to the
/// connection that attached it, so attach, poll and input must share it.
/// Native-terminal launcher kept independent from daemon terminal streaming.
///
/// This mirrors v1's detached platform launcher: `terminal new` must still
/// work while an embedded terminal's daemon port is owned by a launch worker.
struct PlatformExternalTerminalPort;

impl ExternalTerminalPort for PlatformExternalTerminalPort {
    #[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=presentation::tests::external_terminal_launch_does_not_require_agent_port
    fn open(&mut self, directory: &Path) -> Result<(), String> {
        let directory = directory.to_string_lossy().into_owned();
        let argv = if cfg!(target_os = "macos") {
            vec!["open", "-a", "Terminal", &directory]
        } else if cfg!(target_os = "windows") {
            vec!["wt", "-d", &directory]
        } else if cfg!(unix) {
            vec!["x-terminal-emulator", "--working-directory", &directory]
        } else {
            return Err(
                "opening an external terminal is not supported on this platform".to_owned(),
            );
        };
        let (command, arguments) = argv
            .split_first()
            .expect("external terminal command is never empty");
        Command::new(command)
            .args(arguments)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("failed to open external terminal: {error}"))
    }
}

struct DaemonRestoreConnectionPort {
    epochs: mpsc::Receiver<u64>,
    cancelled: Arc<AtomicBool>,
}

#[derive(Clone)]
struct DaemonRestoreConnectionPublisher {
    epochs: mpsc::SyncSender<u64>,
    next_epoch: Arc<AtomicU64>,
    cancelled: Arc<AtomicBool>,
    data_dir: PathBuf,
}

impl DaemonRestoreConnectionPort {
    fn channel(data_dir: PathBuf) -> (Self, DaemonRestoreConnectionPublisher) {
        let (sender, epochs) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        (
            Self {
                epochs,
                cancelled: Arc::clone(&cancelled),
            },
            DaemonRestoreConnectionPublisher {
                epochs: sender,
                next_epoch: Arc::new(AtomicU64::new(0)),
                cancelled,
                data_dir,
            },
        )
    }
}

impl RestoreConnectionPort for DaemonRestoreConnectionPort {
    fn take_reconnected_epoch(&mut self) -> Option<u64> {
        self.epochs.try_iter().max()
    }
}

impl Drop for DaemonRestoreConnectionPort {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

impl DaemonRestoreConnectionPublisher {
    #[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=passive_restore_socket_eof_emits_one_reconnect_epoch_and_drop_cancels_watchers
    fn watch(&self, stream: std::os::unix::net::UnixStream) -> Arc<AtomicBool> {
        let connection_cancelled = Arc::new(AtomicBool::new(false));
        let local_cancelled = Arc::clone(&connection_cancelled);
        let publisher = self.clone();
        thread::spawn(move || {
            while !publisher.cancelled.load(Ordering::Acquire)
                && !local_cancelled.load(Ordering::Acquire)
                && !unix_stream_closed(&stream)
            {
                thread::sleep(Duration::from_millis(50));
            }
            if publisher.cancelled.load(Ordering::Acquire)
                || local_cancelled.load(Ordering::Acquire)
            {
                return;
            }
            drop(stream);
            while !publisher.cancelled.load(Ordering::Acquire)
                && !local_cancelled.load(Ordering::Acquire)
            {
                if usagi_daemon::infrastructure::unix_transport::connect_current(
                    &publisher.data_dir,
                )
                .is_ok()
                {
                    let epoch = publisher.next_epoch.fetch_add(1, Ordering::AcqRel) + 1;
                    match publisher.epochs.try_send(epoch) {
                        Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
                        Err(mpsc::TrySendError::Disconnected(_)) => return,
                    }
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
        });
        connection_cancelled
    }
}

#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=passive_restore_socket_eof_emits_one_reconnect_epoch_and_drop_cancels_watchers
fn unix_stream_closed(stream: &std::os::unix::net::UnixStream) -> bool {
    let mut byte = 0_u8;
    // SAFETY: `byte` is valid writable storage for one byte and `stream` owns
    // a live descriptor for the duration of this non-consuming MSG_PEEK call.
    let received = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            (&raw mut byte).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    if received == 0 {
        return true;
    }
    if received > 0 {
        return false;
    }
    let error = std::io::Error::last_os_error();
    !matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
    )
}

struct DaemonAgentCommandPort {
    /// The shared attach/input lane. It is deadline-armed like every other lane
    /// ([`crate::runtime::daemon::LaneClient`]) and re-armed per request with
    /// [`TerminalLaneBudget`], so a daemon that stops answering costs the render
    /// thread one action budget instead of freezing the UI.
    terminal: Option<LaneClient>,
    /// Dedicated connection for the stateless `Resize` action, carrying a
    /// per-request read deadline. Kept separate from `terminal` so a timed-out
    /// read only drops this lane and never disturbs the attach/input
    /// connection's subscription or exactly-once ledger. (`Resume` polling runs
    /// on the background `pump` instead of any render-thread connection.)
    poll: Option<LaneClient>,
    /// Foreground poll pump. `Resume` fetches run on its own thread and
    /// connection at a bounded interactive cadence, so the render thread only
    /// drains ready output and never blocks on the daemon. Attach registers a
    /// terminal here; detach removes it.
    pump: TerminalPollPump,
    /// Background scope-inventory pump, present only on the resident stream port
    /// of a workspace. It observes the exit metadata of **detached** background
    /// tabs through per-scope `Inventory` requests on its own thread and
    /// connection; the launch and restore ports watch nothing, so they leave it
    /// unset rather than spawning an idle thread.
    inventory: Option<TerminalInventoryPump>,
    /// Client-local incarnation of the shared `terminal` connection.
    ///
    /// Every subscription this port issues carries the epoch it was taken on.
    /// Dropping the connection advances the epoch, which is what invalidates all
    /// of the panes' subscriptions at once: the daemon released those
    /// attachments with the connection, so each session must attach again before
    /// its next `Input`. The epoch advances at the drop rather than at the next
    /// open so a subscription is never mistaken for current while the connection
    /// that owned it is already gone.
    terminal_epoch: u64,
    /// The subscription each attached terminal is currently fenced by, so a
    /// superseded subscription's release cannot revoke the attachment — or the
    /// pump registration — that replaced it.
    attachments: Vec<(usagi_core::domain::id::TerminalRef, TerminalSubscription)>,
    restore_connection: Option<DaemonRestoreConnectionPublisher>,
    terminal_watch_cancelled: Option<Arc<AtomicBool>>,
}

struct UserAgentTabIntentPort {
    store: Option<FileAgentTabIntentStore>,
}

impl UserAgentTabIntentPort {
    fn new() -> Self {
        Self {
            store: FileAgentTabIntentStore::open_default().ok(),
        }
    }
}

impl AgentTabIntentPort for UserAgentTabIntentPort {
    fn load(&mut self, workspace: WorkspaceId) -> Result<AgentTabIntent, AgentTabIntentError> {
        self.store
            .as_mut()
            .ok_or(AgentTabIntentError::Unavailable)?
            .load(workspace)
    }

    fn mutate(
        &mut self,
        workspace: WorkspaceId,
        expected_revision: u64,
        mutation: AgentTabIntentMutation,
    ) -> Result<AgentTabIntentPortCommit, AgentTabIntentError> {
        self.store
            .as_mut()
            .ok_or(AgentTabIntentError::Unavailable)?
            .mutate(workspace, expected_revision, mutation)
    }
}

impl DaemonAgentCommandPort {
    fn new(pump: TerminalPollPump) -> Self {
        Self {
            terminal: None,
            poll: None,
            pump,
            inventory: None,
            terminal_epoch: 1,
            attachments: Vec::new(),
            restore_connection: None,
            terminal_watch_cancelled: None,
        }
    }

    /// Binds the background scope-inventory lane. Only the workspace's resident
    /// stream port takes one; every other client of this adapter watches no
    /// background tab.
    fn with_inventory_pump(mut self, inventory: TerminalInventoryPump) -> Self {
        self.inventory = Some(inventory);
        self
    }

    fn with_restore_connection(mut self, publisher: DaemonRestoreConnectionPublisher) -> Self {
        self.restore_connection = Some(publisher);
        self
    }

    /// Returns the persistent terminal connection, opening it on first use.
    ///
    /// Opening it may have to bootstrap (and cold-start) a daemon, so the
    /// connection is established under the surface policy budget; each request
    /// then re-arms the lane with its own, far smaller
    /// [`TerminalLaneBudget`].
    #[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=real_pty_entry_resize_quit_and_reattach_restore_terminal
    fn terminal_client(&mut self) -> Result<&mut LaneClient, TerminalError> {
        if self.terminal.is_none() {
            let client =
                crate::runtime::daemon::client(ClientPolicy::tui(), TerminalLaneBudget::CONNECT_MS)
                    .map_err(|_| TerminalError::Unavailable)?;
            if let Some(cancelled) = self.terminal_watch_cancelled.take() {
                cancelled.store(true, Ordering::Release);
            }
            if let Some(publisher) = &self.restore_connection
                && let Ok(stream) = crate::runtime::daemon::lane_socket(&client).try_clone()
            {
                self.terminal_watch_cancelled = Some(publisher.watch(stream));
            }
            self.terminal = Some(client);
        }
        Ok(self
            .terminal
            .as_mut()
            .expect("terminal client was just set"))
    }

    /// Returns the terminal lane with a fresh end-to-end budget armed for one
    /// `action`. Arming happens per request rather than per connection because
    /// the lane is persistent: it owns every pane's attachment and the
    /// exactly-once input ledger, so it is never silently replaced the way a
    /// reconnecting per-request client would be.
    #[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=an_unanswered_input_returns_within_the_lane_budget_and_advances_the_epoch
    fn armed_terminal_client(
        &mut self,
        action: TerminalAction,
    ) -> Result<&mut LaneClient, TerminalError> {
        let client = self.terminal_client()?;
        crate::runtime::daemon::rearm_lane(client, TerminalLaneBudget::for_action(action));
        Ok(client)
    }

    /// Whether the connected daemon keeps the durable input operation ledger.
    ///
    /// The capability is the truth source, so a daemon that does not advertise it
    /// is treated as legacy even if it would accept the field: this client then
    /// neither issues an operation identity nor claims a lost acknowledgement is
    /// resolvable (#519).
    fn terminal_input_is_durable(&mut self) -> Result<bool, TerminalError> {
        Ok(self.terminal_client()?.terminal_input_replay_mode()
            == TerminalInputReplayMode::DurableOperation)
    }

    /// Sends one terminal request over the persistent connection and returns its
    /// success body.
    ///
    /// Only a **transport** failure drops the connection: the stream's position
    /// is then unknown, so it cannot be reused. That now includes an exceeded
    /// [`TerminalLaneBudget`], which is exactly the same condition — a socket
    /// that may hold a partial frame. A fully received protocol error means the
    /// daemon answered on a healthy socket — that request is finished, and one
    /// pane's `resync_required` / `stale_target` must not revoke the attachments
    /// every other pane holds on the same connection.
    #[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=real_pty_entry_resize_quit_and_reattach_restore_terminal
    fn terminal_request(
        &mut self,
        action: TerminalAction,
        request: TerminalRequest,
    ) -> Result<serde_json::Value, TerminalError> {
        let payload = serde_json::to_value(request).expect("terminal request is serializable");
        let reply = {
            let client = self.armed_terminal_client(action)?;
            client.request(DaemonRequest::Terminal { action, payload })
        };
        match reply {
            Ok(DaemonReply::Ok(body) | DaemonReply::Accepted { body, .. }) => Ok(body),
            Err(error) => {
                if error.is_transport_failure() {
                    self.reset_terminal();
                }
                Err(map_terminal_error(&error))
            }
        }
    }

    /// Drops the shared connection and advances the epoch, invalidating every
    /// subscription taken on it. The daemon releases those attachments when the
    /// connection closes, so each session attaches freshly instead of fencing
    /// its next input with an attachment that no longer exists.
    fn reset_terminal(&mut self) {
        if let Some(cancelled) = self.terminal_watch_cancelled.take() {
            cancelled.store(true, Ordering::Release);
        }
        self.terminal = None;
        self.terminal_epoch = self
            .terminal_epoch
            .checked_add(1)
            .expect("terminal connection epoch exhausted");
    }

    /// Appends one line per degraded observation lane to the daily error log.
    /// Both lanes run off the render thread, so their failures never reach the
    /// UI; this is what makes a stalled pane inspectable afterwards.
    #[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=terminal_pump_unit_contract
    fn record_lane_degradation(&self) {
        let inventory = self
            .inventory
            .as_ref()
            .and_then(|inventory| inventory.metrics().degradation_summary());
        for summary in self
            .pump
            .metrics()
            .degradation_summary()
            .into_iter()
            .chain(inventory)
        {
            ErrorLog::record(&summary);
        }
    }

    /// Records the subscription a fresh attach fenced `terminal` with, replacing
    /// any superseded one.
    fn record_attachment(
        &mut self,
        terminal: &usagi_core::domain::id::TerminalRef,
        subscription: TerminalSubscription,
    ) {
        self.attachments
            .retain(|(attached, _)| !attached.fences(terminal));
        self.attachments.push((terminal.clone(), subscription));
    }

    /// Whether `subscription` is still the one fencing `terminal`, removing the
    /// record when it is. A superseded subscription is not: a later attach
    /// already replaced it, and releasing it must leave that attachment — and
    /// the pump registration taken with it — in place.
    fn release_attachment(
        &mut self,
        terminal: &usagi_core::domain::id::TerminalRef,
        subscription: TerminalSubscription,
    ) -> bool {
        let current = self
            .attachments
            .iter()
            .any(|(attached, held)| attached.fences(terminal) && *held == subscription);
        if current {
            self.attachments
                .retain(|(attached, _)| !attached.fences(terminal));
        }
        current
    }

    /// Returns the deadline-bounded poll connection, opening it on first use.
    /// `Resume`/`Resize` are stateless on the daemon (keyed only by terminal id
    /// and offset), so this lane never attaches, never carries an input
    /// subscription, and reports no `connection_epoch`.
    #[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=real_pty_entry_resize_quit_and_reattach_restore_terminal
    fn poll_client(&mut self, action: TerminalAction) -> Result<&mut LaneClient, TerminalError> {
        if self.poll.is_none() {
            self.poll = Some(
                crate::runtime::daemon::client(ClientPolicy::tui(), TerminalLaneBudget::CONNECT_MS)
                    .map_err(|_| TerminalError::Unavailable)?,
            );
        }
        let client = self.poll.as_mut().expect("poll client was just set");
        // Bound the whole request so a momentarily busy daemon cannot stall the
        // render thread. A timed-out exchange leaves an unread frame on the
        // socket, so `poll_request` drops the lane on any error.
        crate::runtime::daemon::rearm_lane(client, TerminalLaneBudget::for_action(action));
        Ok(client)
    }

    /// Sends one `Resume`/`Resize` request over the deadline-bounded poll lane.
    /// Any transport failure (including a read timeout) drops this lane only; the
    /// attach/input connection and its exactly-once ledger are untouched.
    #[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=real_pty_entry_resize_quit_and_reattach_restore_terminal
    fn poll_request(
        &mut self,
        action: TerminalAction,
        request: TerminalRequest,
    ) -> Result<serde_json::Value, TerminalError> {
        let payload = serde_json::to_value(request).expect("terminal request is serializable");
        let reply = {
            let client = self.poll_client(action)?;
            client.request(DaemonRequest::Terminal { action, payload })
        };
        match reply {
            Ok(DaemonReply::Ok(body) | DaemonReply::Accepted { body, .. }) => Ok(body),
            Err(error) => {
                self.poll = None;
                Err(map_terminal_error(&error))
            }
        }
    }
}

impl Drop for DaemonAgentCommandPort {
    /// Release the shared connection, then record the observation lanes'
    /// counters when something actually degraded. A wedged pane leaves no trace
    /// otherwise: the lanes complete off the render thread, so their failures are
    /// invisible to the UI by design.
    fn drop(&mut self) {
        self.reset_terminal();
        self.record_lane_degradation();
    }
}

/// Maps a typed client failure onto the safe terminal feedback the UI renders.
/// No mapping authorizes a local PTY fallback.
fn map_terminal_error(error: &usagi_core::usecase::client::ClientError) -> TerminalError {
    use usagi_core::infrastructure::ipc::ErrorCode;
    match error.code() {
        ErrorCode::ResyncRequired => TerminalError::ResyncRequired,
        ErrorCode::StaleTarget => TerminalError::Stale,
        ErrorCode::OwnershipUnknown => TerminalError::Orphaned,
        _ => TerminalError::Unavailable,
    }
}

const MAX_CACHED_INPUT_ACK_DEPTH: usize = 16;

/// Decodes the terminal owner's sequence-consuming input acknowledgement.
///
/// The wire enum is deliberately validated here instead of being collapsed to
/// a body-less success. Unknown variants, malformed partial-write counts and
/// pathological cached nesting all have an unknown effect and therefore fail
/// closed without authorizing a retry.
fn decode_terminal_input_ack(
    body: &serde_json::Value,
    input_len: usize,
) -> Result<TerminalInputOutcome, TerminalError> {
    let object = body
        .as_object()
        .filter(|object| object.len() == 1)
        .ok_or(TerminalError::InputEffectUnknown)?;
    let ack = object.get("ack").ok_or(TerminalError::InputEffectUnknown)?;
    decode_terminal_input_ack_value(ack, input_len, 0)
}

fn decode_terminal_input_ack_value(
    ack: &serde_json::Value,
    input_len: usize,
    cached_depth: usize,
) -> Result<TerminalInputOutcome, TerminalError> {
    match ack.as_str() {
        Some("Written") => return Ok(TerminalInputOutcome::Written),
        Some("Failed") => return Ok(TerminalInputOutcome::Failed),
        Some(_) => return Err(TerminalError::InputEffectUnknown),
        None => {}
    }

    let variant = ack
        .as_object()
        .filter(|object| object.len() == 1)
        .ok_or(TerminalError::InputEffectUnknown)?;
    if let Some(ambiguous) = variant.get("Ambiguous") {
        let fields = ambiguous
            .as_object()
            .filter(|object| object.len() == 1)
            .ok_or(TerminalError::InputEffectUnknown)?;
        let applied_prefix = fields
            .get("applied_prefix")
            .and_then(serde_json::Value::as_u64)
            .and_then(|prefix| usize::try_from(prefix).ok())
            .filter(|prefix| *prefix > 0 && *prefix <= input_len)
            .ok_or(TerminalError::InputEffectUnknown)?;
        return Ok(TerminalInputOutcome::Ambiguous { applied_prefix });
    }
    if let Some(cached) = variant.get("Cached") {
        if cached_depth >= MAX_CACHED_INPUT_ACK_DEPTH {
            return Err(TerminalError::InputEffectUnknown);
        }
        return decode_terminal_input_ack_value(cached, input_len, cached_depth + 1);
    }
    Err(TerminalError::InputEffectUnknown)
}

fn decode_terminal_inventory(
    body: &serde_json::Value,
) -> Result<Vec<usagi_core::domain::terminal_launch::TerminalInventoryEntry>, TerminalError> {
    body.get("terminals")
        .and_then(serde_json::Value::as_array)
        .ok_or(TerminalError::Unavailable)?
        .iter()
        .map(|item| serde_json::from_value(item.clone()).map_err(|_| TerminalError::Unavailable))
        .collect()
}

/// Decode the screen an attach / resync snapshot carries, according to the
/// contract the connection negotiated.
///
/// On the checkpoint path the frame must satisfy `base_offset == output_offset`
/// (a checkpoint is complete at `output_offset`, so it has no tail) and carry a
/// checkpoint this build accepts; anything else is refused rather than displayed.
/// On the legacy path the retained `replay` tail is **not read at all**: a tail
/// cut mid UTF-8 / CSI / OSC must never reach a parser, so the client fails
/// closed to a history-less view and renders only output after `output_offset`.
fn decode_attach_screen(
    mode: TerminalSnapshotMode,
    snapshot: &serde_json::Value,
    base_offset: u64,
    output_offset: u64,
) -> Result<TerminalAttachScreen, TerminalError> {
    match mode {
        TerminalSnapshotMode::Checkpoint => {
            if base_offset != output_offset {
                return Err(TerminalError::Unavailable);
            }
            // The frame is already bounded by the negotiated IPC frame limit;
            // the checkpoint's own bounds are enforced when it is restored.
            let checkpoint: ScreenCheckpoint = serde_json::from_value(snapshot["screen"].clone())
                .map_err(|_| TerminalError::Unavailable)?;
            Ok(TerminalAttachScreen::Checkpoint(Box::new(checkpoint)))
        }
        TerminalSnapshotMode::LegacyFailClosed => Ok(TerminalAttachScreen::HistoryUnavailable),
    }
}

fn terminal_inventory_matches_scope(
    entries: &[usagi_core::domain::terminal_launch::TerminalInventoryEntry],
    scope: &TerminalLaunchScope,
) -> bool {
    entries.iter().all(|entry| {
        entry.terminal.workspace_id == scope.workspace_id
            && entry.terminal.session_id == scope.session_id
            && entry.terminal.worktree_id == scope.worktree_id
    })
}

fn agent_inventory_request(workspace: WorkspaceId) -> DaemonRequest {
    DaemonRequest::AgentInventory { workspace }
}

fn exact_agent_resume_request(
    operation_id: usagi_core::domain::id::OperationId,
    target: usagi_core::domain::agent::AgentResumeTarget,
) -> DaemonRequest {
    DaemonRequest::ResumeAgent {
        operation_id: operation_id.to_string(),
        target,
    }
}

/// Decode one exact-target resume answer, keeping the daemon's own lineage and
/// source-to-replacement relation. Nothing is inferred here: a body without a
/// decodable relation yields `None` and the TUI refuses the replacement (#510).
fn decode_exact_agent_resume(body: &serde_json::Value) -> Result<ExactAgentResume, String> {
    let terminal = body
        .get("terminal")
        .cloned()
        .ok_or_else(|| "provider resume returned no terminal".to_owned())
        .and_then(|terminal| {
            serde_json::from_value(terminal)
                .map_err(|_| "provider resume returned an invalid terminal".to_owned())
        })?;
    let continuation = body
        .get("continuation")
        .filter(|value| !value.is_null())
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok());
    let relation = body
        .get("resume_relation")
        .filter(|value| !value.is_null())
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok());
    Ok(ExactAgentResume {
        terminal,
        continuation,
        relation,
    })
}

fn decode_agent_admission(
    body: &serde_json::Value,
    operation: &str,
) -> Result<AgentPaneAdmission, String> {
    let terminal = body
        .get("terminal")
        .cloned()
        .ok_or_else(|| format!("{operation} returned no terminal"))
        .and_then(|terminal| {
            serde_json::from_value(terminal)
                .map_err(|_| format!("{operation} returned an invalid terminal"))
        })?;
    let continuation = body
        .get("continuation")
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|_| format!("{operation} returned an invalid continuation"))?;
    Ok(AgentPaneAdmission {
        terminal,
        continuation,
    })
}

#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=daemon_terminal_decode_and_reconnect_contract
impl AgentCommandPort for DaemonAgentCommandPort {
    fn launch(
        &mut self,
        workspace: WorkspaceId,
        session: Option<usagi_core::domain::id::SessionId>,
        profile: Option<usagi_core::domain::agent::AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        let mut client =
            crate::runtime::daemon::policy_client(usagi_core::usecase::client::ClientPolicy::tui())
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
            DaemonReply::Accepted { body, .. } | DaemonReply::Ok(body) => {
                decode_agent_admission(&body, "agent launch")
            }
        }
    }

    #[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=structured_codex_identity_enables_one_explicit_new_runtime_resume
    fn resume(
        &mut self,
        _workspace: WorkspaceId,
        session: usagi_core::domain::id::SessionId,
        operation_id: usagi_core::domain::id::OperationId,
    ) -> Result<AgentPaneAdmission, String> {
        let mut client =
            crate::runtime::daemon::policy_client(usagi_core::usecase::client::ClientPolicy::tui())
                .map_err(|_| "daemon unavailable; reconnect to continue".to_owned())?;
        match client
            .request(DaemonRequest::Session {
                action: SessionAction::ResumeAgent,
                operation_id: operation_id.to_string(),
                payload: serde_json::json!({"session_id": session}),
            })
            .map_err(|_| "provider resume failed; inspect session status".to_owned())?
        {
            DaemonReply::Accepted { body, .. } | DaemonReply::Ok(body) => {
                decode_agent_admission(&body, "provider resume")
            }
        }
    }

    fn resume_inventory(
        &mut self,
        workspace: WorkspaceId,
    ) -> Result<usagi_core::domain::agent::AgentInventory, String> {
        let mut client =
            crate::runtime::daemon::policy_client(usagi_core::usecase::client::ClientPolicy::tui())
                .map_err(|_| "daemon unavailable; reconnect to continue".to_owned())?;
        match client
            .request(agent_inventory_request(workspace))
            .map_err(|_| "Agent resume inventory is unavailable".to_owned())?
        {
            DaemonReply::Accepted { body, .. } | DaemonReply::Ok(body) => {
                serde_json::from_value(body)
                    .map_err(|_| "daemon returned an invalid Agent inventory".to_owned())
            }
        }
    }

    fn resume_exact(
        &mut self,
        target: usagi_core::domain::agent::AgentResumeTarget,
        operation_id: usagi_core::domain::id::OperationId,
    ) -> Result<ExactAgentResume, String> {
        let mut client =
            crate::runtime::daemon::policy_client(usagi_core::usecase::client::ClientPolicy::tui())
                .map_err(|_| "daemon unavailable; reconnect to continue".to_owned())?;
        match client
            .request(exact_agent_resume_request(operation_id, target))
            .map_err(|_| "provider resume failed; refresh Agent inventory".to_owned())?
        {
            DaemonReply::Accepted { body, .. } | DaemonReply::Ok(body) => {
                decode_exact_agent_resume(&body)
            }
        }
    }

    fn launch_terminal(
        &mut self,
        workspace: WorkspaceId,
        session: Option<usagi_core::domain::id::SessionId>,
        geometry: usagi_tui::usecase::application::pane_runtime::Geometry,
        arguments: &str,
        operation: usagi_core::domain::id::OperationId,
    ) -> Result<usagi_core::domain::id::TerminalRef, String> {
        if !matches!(arguments, "open" | "new") {
            return Err("terminal accepts only `open` or `new`".to_owned());
        }
        if arguments == "open"
            && let Some(existing) = self
                .list_terminals()
                .map_err(|_| "daemon unavailable; reconnect to continue".to_owned())?
                .into_iter()
                .find(|entry| {
                    entry.live
                        && entry.kind == usagi_core::domain::terminal_launch::TerminalKind::Terminal
                        && entry.terminal.workspace_id == workspace
                        && entry.terminal.session_id == session
                })
        {
            return Ok(existing.terminal);
        }
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
            // The controller's durable operation reaches the daemon unchanged, so
            // a lost response or a reconnect replays the same terminal.
            launch_operation: Some(operation),
        };
        let mut client = crate::runtime::daemon::policy_client(ClientPolicy::tui())
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

    fn list_terminals(
        &mut self,
    ) -> Result<
        Vec<usagi_core::domain::terminal_launch::TerminalInventoryEntry>,
        usagi_tui::usecase::application::terminal_session::TerminalError,
    > {
        use usagi_core::domain::session_lifecycle::SessionLifecycle;
        use usagi_core::domain::terminal_launch::TerminalLaunchScope;
        use usagi_tui::usecase::application::terminal_session::TerminalError;

        let lifecycle = request_lifecycle_snapshot().map_err(|_| TerminalError::Unavailable)?;
        // The client never invents a worktree identity: scopes come from the
        // daemon snapshot — the published root worktree and each available
        // session's worktree — and the daemon re-validates them. Sessions the
        // snapshot does not list as available are skipped, so a stale or
        // recreated session's old runtime is never rediscovered.
        let mut launch_scopes = vec![TerminalLaunchScope {
            workspace_id: lifecycle.workspace_id,
            session_id: None,
            worktree_id: lifecycle.root_worktree_id,
        }];
        for managed in &lifecycle.sessions {
            if managed.lifecycle == SessionLifecycle::Available {
                launch_scopes.push(TerminalLaunchScope {
                    workspace_id: lifecycle.workspace_id,
                    session_id: Some(managed.session_id),
                    worktree_id: managed.worktree_id,
                });
            }
        }
        let mut entries = Vec::new();
        for scope in launch_scopes {
            let body = self.terminal_request(
                TerminalAction::Inventory,
                TerminalRequest::Inventory {
                    scope: scope.clone(),
                },
            )?;
            let scope_entries = decode_terminal_inventory(&body)?;
            if !terminal_inventory_matches_scope(&scope_entries, &scope) {
                return Err(TerminalError::Unavailable);
            }
            entries.extend(scope_entries);
        }
        Ok(entries)
    }

    fn attach_terminal(
        &mut self,
        terminal: &usagi_core::domain::id::TerminalRef,
        _geometry: Geometry,
    ) -> Result<TerminalAttach, TerminalError> {
        // The negotiated connection decides the snapshot contract before the
        // request is sent, so a daemon without the checkpoint capability can
        // never have its raw tail decoded into a screen.
        let mode = self.terminal_client()?.terminal_snapshot_mode();
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
        let base_offset = snapshot["base_offset"]
            .as_u64()
            .ok_or(TerminalError::Unavailable)?;
        let revision = snapshot["revision"]
            .as_u64()
            .ok_or(TerminalError::Unavailable)?;
        let screen = decode_attach_screen(mode, snapshot, base_offset, output_offset)?;
        // `exited` is `Option<i32>`: null while the process is still running.
        let exited = !snapshot["exited"].is_null();
        // The snapshot was served by the connection this port currently holds, so
        // the subscription belongs to its epoch. A transport failure would have
        // advanced the epoch before returning, never after.
        let subscription = TerminalSubscription {
            id: subscription,
            epoch: self.terminal_epoch,
        };
        self.record_attachment(terminal, subscription);
        // Resume polling for this terminal on the foreground pump from the
        // snapshot's output offset, fenced by the epoch this attach was served
        // on. Reattach (after a reconnect/resync) resets the pump to the fresh
        // offset, so an in-flight fetch of the previous registration is dropped
        // instead of rewinding the resynced cursor.
        self.pump
            .register(terminal, output_offset, self.terminal_epoch);
        Ok(TerminalAttach {
            subscription,
            revision,
            output_offset,
            screen,
            exited,
        })
    }

    fn resize_terminal(
        &mut self,
        terminal: &usagi_core::domain::id::TerminalRef,
        geometry: Geometry,
    ) -> Result<(), TerminalError> {
        // A resize reflows the screen immediately, so restore the interactive
        // fetch cadence rather than waiting out an idle backoff.
        self.pump.wake();
        self.poll_request(
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

    fn poll_terminal(
        &mut self,
        terminal: &usagi_core::domain::id::TerminalRef,
        after_offset: u64,
    ) -> Result<Vec<TerminalChunk>, TerminalError> {
        // Non-blocking: drain whatever the background pump has already fetched.
        // The daemon `Resume` IPC happens on the pump thread, so a busy daemon
        // never stalls the render/input loop.
        self.pump.take(terminal, after_offset)
    }

    fn terminal_connection_epoch(&self) -> Option<u64> {
        Some(self.terminal_epoch)
    }

    fn input_terminal(
        &mut self,
        terminal: &usagi_core::domain::id::TerminalRef,
        subscription: TerminalSubscription,
        input_seq: u64,
        operation: usagi_core::domain::id::OperationId,
        bytes: &[u8],
    ) -> Result<TerminalInputOutcome, TerminalError> {
        // Backstop for the session's own epoch fence: an input fenced by a
        // replaced connection's subscription is refused here rather than written
        // to the current one, where the daemon would reject it as unattached and
        // the keystroke would be spent for nothing. Nothing is sent, so the
        // effect is definitely zero and the session reattaches.
        if subscription.epoch != self.terminal_epoch {
            return Err(TerminalError::Unavailable);
        }
        // The keystroke is about to produce output: restore the interactive
        // fetch cadence so the echo is not delayed by an idle backoff.
        self.pump.wake();
        // The operation identity is sent only to a daemon that advertises the
        // durable ledger. Sending it to a peer that ignores it would let this
        // client believe a lost acknowledgement is resolvable when it is not.
        let durable = self.terminal_input_is_durable()?;
        let payload = serde_json::to_value(TerminalRequest::Input {
            terminal: terminal.clone(),
            subscription: subscription.id,
            input_seq,
            input_operation: durable.then_some(operation),
            bytes: bytes.to_vec(),
        })
        .expect("terminal request is serializable");
        let reply = {
            // Failure to establish a connection happens before this request is
            // written, so it remains a definite unavailable outcome.
            let client = self.armed_terminal_client(TerminalAction::Input)?;
            client.request(DaemonRequest::Terminal {
                action: TerminalAction::Input,
                payload,
            })
        };
        // A fully received answer — an `Ok` body this build cannot decode, a
        // non-final `Accepted`, or a protocol error — leaves the stream
        // consistent, so the connection and the daemon's input ledger for it are
        // kept. Only a transport failure drops the connection (and with it every
        // pane's subscription), because only then is the stream's position
        // unknown.
        match reply {
            Ok(DaemonReply::Ok(body)) => decode_terminal_input_ack(&body, bytes.len()),
            // `Accepted` is not a final input ACK. Likewise, once the request
            // write was attempted, an EOF or transport error can equally mean
            // "not sent", "partly sent" or "applied but ACK lost". Never label
            // either path undelivered and never replay it blindly.
            Ok(DaemonReply::Accepted { .. }) => Err(TerminalError::InputEffectUnknown),
            // A lane deadline overrun arrives here as a transport failure. It is
            // deliberately *not* retried: the daemon may already have written
            // the bytes to the PTY, so the keystroke's effect is unknown and the
            // session resolves it with the read-only `InputOutcome` query
            // instead of sending it again (#519).
            Err(
                error @ (ClientError::Unavailable(_)
                | ClientError::Lifecycle(_)
                | ClientError::RolloverRequired(_)
                | ClientError::BuildIdentityUnavailable
                | ClientError::BootstrapContended),
            ) => {
                if error.is_transport_failure() {
                    self.reset_terminal();
                }
                Err(TerminalError::InputEffectUnknown)
            }
            Err(error @ ClientError::Protocol(_)) => {
                if error.side_effect() == usagi_core::infrastructure::ipc::SideEffect::None {
                    Err(map_terminal_error(&error))
                } else {
                    Err(TerminalError::InputEffectUnknown)
                }
            }
        }
    }

    fn terminal_input_outcome(
        &mut self,
        terminal: &usagi_core::domain::id::TerminalRef,
        operation: usagi_core::domain::id::OperationId,
        input_len: usize,
    ) -> Result<TerminalInputResolution, TerminalError> {
        // A daemon without the ledger cannot answer, and a guess is exactly what
        // must not happen here: the caller keeps its uncertainty latched.
        if !self.terminal_input_is_durable()? {
            return Ok(TerminalInputResolution::Unknown);
        }
        let body = self.terminal_request(
            TerminalAction::InputOutcome,
            TerminalRequest::InputOutcome {
                terminal: terminal.clone(),
                input_operation: operation,
            },
        )?;
        match body["outcome"].as_str() {
            Some("unknown") => Ok(TerminalInputResolution::Unknown),
            // A recorded final is projected exactly as the lost acknowledgement
            // would have been, success or not, and its applied prefix is bounded
            // by the input it belongs to.
            Some("final") => decode_terminal_input_ack_value(&body["ack"], input_len, 0)
                .map(TerminalInputResolution::Final),
            // An answer this build cannot read is not "nothing happened": it
            // stays an unresolved effect rather than a decoded success.
            _ => Err(TerminalError::InputEffectUnknown),
        }
    }

    fn detach_terminal(
        &mut self,
        terminal: &usagi_core::domain::id::TerminalRef,
        subscription: TerminalSubscription,
    ) {
        // Releasing a superseded subscription must not stop the stream a newer
        // attach established for the same terminal: that attach owns the pump
        // registration now.
        if self.release_attachment(terminal, subscription) {
            self.pump.unregister(terminal);
        }
        // A subscription from a replaced connection is released locally. The
        // daemon dropped that attachment when the connection closed, so sending
        // the request on the current connection would only ask it to release a
        // subscription it never issued.
        if subscription.epoch == self.terminal_epoch {
            let _ = self.terminal_request(
                TerminalAction::Detach,
                TerminalRequest::Detach {
                    terminal: terminal.clone(),
                    subscription: subscription.id,
                },
            );
        }
    }

    fn watch_background_terminals(&mut self, terminals: &[usagi_core::domain::id::TerminalRef]) {
        // Fenced by the shared connection epoch: when the transport is replaced
        // every pane re-attaches, and the background observation bound applies
        // again from the newly available epoch.
        if let Some(inventory) = &self.inventory {
            inventory.watch(self.terminal_epoch, terminals);
        }
    }

    fn take_exited_background_terminals(
        &mut self,
        limit: usize,
    ) -> Vec<usagi_core::domain::id::TerminalRef> {
        self.inventory
            .as_ref()
            .map(|inventory| inventory.take_exited(limit))
            .unwrap_or_default()
    }
}

/// Cadence of Home's durable-decision lane. A pending decision blocks an agent,
/// so this is the most responsive of the three; the bound is what matters, not
/// the exact value.
fn decision_cadence() -> RefreshCadence {
    RefreshCadence::new(
        Duration::from_millis(500),
        Duration::from_millis(500),
        Duration::from_millis(8_000),
    )
}

/// Cadence of Home's session-inventory lane. It only has to notice lifecycle
/// changes another client made (an MCP server creating a session); anything the
/// user does here is adopted from the command's own result or an explicit wake,
/// so a one-second worst case for a foreign change is the accepted visible cost
/// of coalescing (#551).
fn session_cadence() -> RefreshCadence {
    RefreshCadence::new(
        Duration::from_millis(1_000),
        Duration::from_millis(1_000),
        Duration::from_millis(8_000),
    )
}

/// Cadence of the mascot's metrics lane. It matches the one-second throttle the
/// inline port already applied, so the sampling rate is unchanged and only the
/// thread it runs on differs.
fn metrics_cadence() -> RefreshCadence {
    RefreshCadence::new(
        Duration::from_millis(1_000),
        Duration::from_millis(1_000),
        Duration::from_millis(8_000),
    )
}

/// How many times a lane that is allowed to cold-start may pay for one, per
/// workspace launch. Cold-start runs a lifecycle subprocess and can sleep out a
/// two-second readiness wait, so an observation lane retrying it every cadence
/// period would keep the shared bootstrap lock hot forever (#551).
const LANE_COLD_START_BUDGET: u32 = 3;

/// One background observation lane's persistent daemon connection.
///
/// The lane opens it once and keeps it for the whole workspace, so the steady
/// state costs one request per cadence period instead of a full
/// `bootstrap_client` (data-dir `flock` ×2, `bootstrap.lock`, `current_exe`,
/// locator read, handshake) per frame tick (#551). A transport failure drops the
/// connection so the next round reconnects; the pump's backoff bounds how often
/// that happens.
///
/// Cold-start authority is a property of the lane, decided once here:
/// [`Self::observing`] never starts a daemon, while [`Self::lifecycle`] may pay
/// for a bounded number of cold starts when — and only when — a plain attach
/// fails.
struct LaneConnection {
    client: Option<Box<dyn DaemonClient + Send>>,
    cold_start_budget: u32,
}

impl LaneConnection {
    /// A display/observation lane: it reports the daemon's absence and never
    /// starts one.
    const fn observing() -> Self {
        Self {
            client: None,
            cold_start_budget: 0,
        }
    }

    /// The session-lifecycle lane: the one lane whose data the user acts on, so
    /// it is the one allowed to bring a missing daemon back.
    const fn lifecycle() -> Self {
        Self {
            client: None,
            cold_start_budget: LANE_COLD_START_BUDGET,
        }
    }

    /// Perform one request on the lane's connection, opening it first when
    /// needed and dropping it on any transport failure.
    #[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=refresh_pump_lane_contract
    fn request(&mut self, request: DaemonRequest) -> Result<DaemonReply, String> {
        if self.client.is_none() {
            self.client = Some(self.connect()?);
        }
        match self
            .client
            .as_mut()
            .expect("the lane connection was just opened")
            .request(request)
        {
            Ok(reply) => Ok(reply),
            Err(error) => {
                self.client = None;
                Err(daemon_error_reason(error))
            }
        }
    }

    /// Attach to a running daemon; only a lane with cold-start budget falls back
    /// to the bootstrapping client, and each fallback spends one unit of it.
    #[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=refresh_pump_lane_contract
    fn connect(&mut self) -> Result<Box<dyn DaemonClient + Send>, String> {
        match crate::runtime::daemon::attached_client(ClientPolicy::tui()) {
            Ok(client) => Ok(Box::new(client)),
            Err(error) if self.cold_start_budget == 0 => {
                Err(format!("daemon unavailable: {error}"))
            }
            Err(_) => {
                self.cold_start_budget -= 1;
                crate::runtime::daemon::policy_client(ClientPolicy::tui())
                    .map(|client| Box::new(client) as Box<dyn DaemonClient + Send>)
                    .map_err(|error| format!("daemon unavailable: {error}"))
            }
        }
    }
}

/// Spawns Home's resident durable-decision lane. Observation only: a missing
/// daemon is reported, never started (#551).
#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=refresh_pump_lane_contract
fn spawn_decision_pump() -> RefreshPump<Vec<usagi_core::domain::user_decision::UserDecision>> {
    let mut lane = LaneConnection::observing();
    RefreshPump::spawn(decision_cadence(), move || {
        let reply = lane.request(DaemonRequest::UserDecision {
            action: usagi_core::usecase::client::TuiUserDecisionAction::List,
            payload: serde_json::json!({}),
        })?;
        let DaemonReply::Ok(value) = reply else {
            return Err("daemon did not return a decision snapshot".to_owned());
        };
        serde_json::from_value(value.get("decisions").cloned().unwrap_or(value))
            .map_err(|_| "daemon returned an invalid decision snapshot".to_owned())
    })
}

/// Spawns Home's resident session-inventory lane on its own connection, so a
/// slow user-initiated create/remove and the background observation never block
/// each other (#551).
#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=refresh_pump_lane_contract
fn spawn_session_refresh_pump(workspace: Workspace) -> RefreshPump<SessionCommandResult> {
    let mut lane = LaneConnection::lifecycle();
    RefreshPump::spawn(session_cadence(), move || {
        let reply = lane.request(DaemonRequest::Session {
            action: SessionAction::List,
            operation_id: usagi_core::domain::id::OperationId::new().to_string(),
            payload: serde_json::json!({}),
        })?;
        let (DaemonReply::Ok(body) | DaemonReply::Accepted { body, .. }) = reply;
        let snapshot = lifecycle_snapshot(&body)?;
        // The `state.json` read this performs is workspace-local file IO; it
        // belongs on this thread with the request, not on the render thread.
        session_snapshot_result("daemon snapshot refreshed", &snapshot, &workspace)
    })
}

/// Spawns the mascot's resident metrics lane. Display-only, so like
/// `observation_client` before it, it never cold-starts a daemon (#551).
#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=refresh_pump_lane_contract
fn spawn_metrics_pump() -> RefreshPump<Option<DaemonMetrics>> {
    let mut lane = LaneConnection::observing();
    RefreshPump::spawn(metrics_cadence(), move || {
        let reply = lane.request(DaemonRequest::Metrics {
            action: MetricsAction::Snapshot,
        })?;
        match reply {
            DaemonReply::Ok(value) => Ok(serde_json::from_value(value).ok()),
            DaemonReply::Accepted { .. } => {
                Err("daemon did not return a metrics snapshot".to_owned())
            }
        }
    })
}

/// Spawns a background poll pump backed by a dedicated, deadline-bounded daemon
/// connection so every `Resume` fetch runs off the render thread.
#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=real_pty_entry_resize_quit_and_reattach_restore_terminal
fn spawn_poll_pump() -> TerminalPollPump {
    let mut client: Option<LaneClient> = None;
    TerminalPollPump::spawn(move |fence| {
        fetch_terminal_output(&mut client, &fence.terminal, fence.after_offset)
    })
}

/// Spawns the background scope-inventory pump on its own deadline-bounded daemon
/// connection. It is the only observation primitive for detached background
/// tabs: it asks for a scope's inventory and never attaches or resumes one of
/// them (#527).
#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=inventory_pump_unit_contract
fn spawn_inventory_pump() -> TerminalInventoryPump {
    let mut client: Option<LaneClient> = None;
    TerminalInventoryPump::spawn(move |job| fetch_scope_inventory(&mut client, &job.scope))
}

/// Performs one scope `Inventory` request on the inventory lane's own
/// connection, reconnecting on any transport error (including a read timeout).
/// Called only by the inventory pump thread, never the render thread.
#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=inventory_pump_unit_contract
fn fetch_scope_inventory(
    client: &mut Option<LaneClient>,
    scope: &TerminalLaunchScope,
) -> Result<Vec<usagi_core::domain::terminal_launch::TerminalInventoryEntry>, ()> {
    if client.is_none() {
        *client = Some(
            crate::runtime::daemon::client(ClientPolicy::tui(), TerminalLaneBudget::CONNECT_MS)
                .map_err(|_| ())?,
        );
    }
    crate::runtime::daemon::rearm_lane(
        client.as_mut().expect("inventory connection was just set"),
        TerminalLaneBudget::for_action(TerminalAction::Inventory),
    );
    let payload = serde_json::to_value(TerminalRequest::Inventory {
        scope: scope.clone(),
    })
    .expect("terminal request is serializable");
    let reply = client
        .as_mut()
        .expect("inventory connection was just set")
        .request(DaemonRequest::Terminal {
            action: TerminalAction::Inventory,
            payload,
        });
    match reply {
        Ok(DaemonReply::Ok(body) | DaemonReply::Accepted { body, .. }) => {
            decode_terminal_inventory(&body).map_err(|_| ())
        }
        Err(_) => {
            *client = None;
            Err(())
        }
    }
}

/// Performs one `Resume` fetch on the pump's own deadline-bounded connection,
/// reconnecting on any transport error (including a read timeout). Called only
/// by the background pump thread, never the render thread.
#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=real_pty_entry_resize_quit_and_reattach_restore_terminal
fn fetch_terminal_output(
    client: &mut Option<LaneClient>,
    terminal: &usagi_core::domain::id::TerminalRef,
    after_offset: u64,
) -> Result<Vec<TerminalChunk>, TerminalError> {
    if client.is_none() {
        *client = Some(
            crate::runtime::daemon::client(ClientPolicy::tui(), TerminalLaneBudget::CONNECT_MS)
                .map_err(|_| TerminalError::Unavailable)?,
        );
    }
    crate::runtime::daemon::rearm_lane(
        client.as_mut().expect("poll connection was just set"),
        TerminalLaneBudget::for_action(TerminalAction::Resume),
    );
    let payload = serde_json::to_value(TerminalRequest::Resume {
        terminal: terminal.clone(),
        after_offset,
    })
    .expect("terminal request is serializable");
    let reply = client
        .as_mut()
        .expect("poll connection was just set")
        .request(DaemonRequest::Terminal {
            action: TerminalAction::Resume,
            payload,
        });
    match reply {
        Ok(DaemonReply::Ok(body) | DaemonReply::Accepted { body, .. }) => {
            decode_terminal_poll(&body)
        }
        Err(error) => {
            *client = None;
            Err(map_terminal_error(&error))
        }
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
    agent_resumes: BTreeMap<SessionId, ProviderResumeProjection>,
}

impl LifecycleSnapshot {
    /// Sessions the sidebar lists: usable `Available` checkouts, `Failed`
    /// reservations, and `Deleting` rows whose teardown is still running. A
    /// `Failed` row still owns its name, so listing it is what lets a client see
    /// and remove it; a `Deleting` row is the visible half of an accepted
    /// removal, which the daemon's teardown worker finishes asynchronously and
    /// which therefore lasts as long as the worktree takes to remove. Capability
    /// gating (attach vs remove) is derived per row from its lifecycle, so
    /// neither row is attachable and a `Deleting` row cannot be removed again.
    /// The reservation states (`Creating` / `Initializing`) stay hidden: they are
    /// bounded by one request and have their own pending-row treatment.
    fn listed_sessions(&self) -> impl Iterator<Item = &ManagedSession> {
        use usagi_core::domain::session_lifecycle::SessionLifecycle;
        self.sessions.iter().filter(|session| {
            matches!(
                session.lifecycle,
                SessionLifecycle::Available | SessionLifecycle::Failed | SessionLifecycle::Deleting
            )
        })
    }

    /// Lifecycle projection for the listed rows, keyed by stable identity. A
    /// `Failed` row carries its safe failure summary; other rows carry `None`.
    fn session_lifecycles(&self) -> BTreeMap<SessionId, SessionLifecycleProjection> {
        self.listed_sessions()
            .map(|session| {
                (
                    session.session_id,
                    SessionLifecycleProjection {
                        lifecycle: session.lifecycle,
                        failure_summary: session
                            .failure
                            .as_ref()
                            .map(|failure| failure.summary.clone()),
                    },
                )
            })
            .collect()
    }

    fn project(&self, workspace: &Workspace, legacy: &[SessionRecord]) -> Vec<SessionRecord> {
        self.listed_sessions()
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

#[coverage(off)] // coverage: reason=generic_monomorphization owner=tui expires=2027-01-31 tests=lifecycle_parser_projection_and_safe_error_mapping_cover_every_branch
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
        let session_values = object
            .get("sessions")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| "daemon session snapshot has no sessions".to_owned())?;
        let agent_resumes = session_values
            .iter()
            .map(provider_resume_projection)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect();
        let sessions = serde_json::from_value(serde_json::Value::Array(session_values.clone()))
            .map_err(|error| format!("invalid daemon session snapshot: {error}"))?;
        Ok(LifecycleSnapshot {
            workspace_id,
            root_worktree_id,
            revision,
            sessions,
            agent_resumes,
        })
    })();
    record_lifecycle_snapshot_error(&result);
    result
}

#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=lifecycle_parser_projection_and_safe_error_mapping_cover_every_branch
fn record_lifecycle_snapshot_error(result: &Result<LifecycleSnapshot, String>) {
    if let Err(error) = result {
        // The daemon snapshot contains no user-supplied argv or environment.
        // Persist only the schema error, never the raw IPC body.
        ErrorLog::record(&format!("daemon lifecycle snapshot rejected: {error}"));
    }
}

fn provider_resume_projection(
    item: &serde_json::Value,
) -> Result<Option<(SessionId, ProviderResumeProjection)>, String> {
    let Some(phase) = item.get("agent_phase") else {
        return Ok(None);
    };
    let phase = phase
        .as_str()
        .ok_or_else(|| "daemon Agent phase is invalid".to_owned())?;
    let session = item
        .get("session_id")
        .cloned()
        .ok_or_else(|| "daemon Agent projection has no session ID".to_owned())
        .and_then(|value| {
            serde_json::from_value(value)
                .map_err(|_| "daemon Agent projection has an invalid session ID".to_owned())
        })?;
    let resumable = item
        .get("agent_resumable")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| "daemon Agent resume availability is invalid".to_owned())?;
    let reason = item
        .get("agent_resume_reason")
        .cloned()
        .ok_or_else(|| "daemon Agent resume reason is missing".to_owned())
        .and_then(|value| {
            serde_json::from_value::<ProviderResumeReason>(value)
                .map_err(|_| "daemon Agent resume reason is invalid".to_owned())
        })?;
    Ok(Some((
        session,
        ProviderResumeProjection {
            interrupted: phase == "interrupted",
            resumable,
            reason,
        },
    )))
}

#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=production_session_completion_contract
impl SessionCommandPort for DaemonSessionCommandPort {
    fn execute(
        &self,
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
            SessionCommand::Resume { .. } => {
                // Resume spawns a runtime the caller must attach to. Only the
                // controller's `ResumeAgent` effect owns that pending pane, so
                // this attach-less port refuses instead of spawning blindly.
                return Err("session resume must be handled by the TUI".to_owned());
            }
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
            crate::runtime::daemon::policy_client(usagi_core::usecase::client::ClientPolicy::tui())
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
                if action == SessionAction::Create {
                    created_session_hook(&body, &operation_id, revision)?;
                }
                session_snapshot_result(
                    format!("completed operation {operation_id} (revision {revision})"),
                    &snapshot,
                    workspace,
                )
            }
            DaemonReply::Ok(value) => {
                let snapshot = lifecycle_snapshot(&value)?;
                session_snapshot_result("daemon snapshot refreshed", &snapshot, workspace)
            }
        }
    }
}

/// Validate the daemon-owned final hook that ends a `session create` loading
/// wave.  A snapshot by itself is not sufficient here: a delayed or unrelated
/// accepted response must not clear the pending skeleton for this operation.
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

fn session_snapshot_result(
    message: impl Into<String>,
    snapshot: &LifecycleSnapshot,
    workspace: &Workspace,
) -> Result<SessionCommandResult, String> {
    // Align the identities with the listed rows (`project` lists the same set),
    // so a `Failed` row joins its stable ID and can be removed. Terminal/Agent
    // scopes still filter to `Available` at their own call sites.
    let session_ids = snapshot
        .listed_sessions()
        .map(|session| session.session_id)
        .collect();
    let legacy = match load_workspace_state(&workspace.path) {
        Ok(state) => state,
        Err(error) => return Err(error.to_string()),
    };
    Ok(SessionCommandResult {
        message: message.into(),
        sessions: Some(snapshot.project(workspace, &legacy.sessions)),
        session_ids: Some(session_ids),
        agent_resumes: Some(snapshot.agent_resumes.clone()),
        session_lifecycles: Some(snapshot.session_lifecycles()),
        revision: Some(snapshot.revision),
    })
}

/// Why a lifecycle snapshot could not be read.
///
/// Every surface but one shows a single line ([`LifecycleRequestError::reason`]).
/// Opening a workspace is the exception: it must tell a daemon that refuses the
/// declared workspace apart from a daemon that is merely unavailable, because
/// only the first one is a workspace the user has to switch to.
enum LifecycleRequestError {
    Connect(ClientError),
    Request(ClientError),
    Decode(String),
}

impl LifecycleRequestError {
    fn reason(self) -> String {
        match self {
            Self::Connect(error) => format!("daemon unavailable: {error}"),
            Self::Request(error) => daemon_error_reason(error),
            Self::Decode(reason) => reason,
        }
    }

    /// The workspace-fence refusal behind this failure, if the daemon answered
    /// that it does not serve the workspace this client declared.
    fn workspace_refusal(&self) -> Option<&usagi_core::infrastructure::ipc::ProtocolError> {
        let (Self::Connect(ClientError::Protocol(error))
        | Self::Request(ClientError::Protocol(error))) = self
        else {
            return None;
        };
        usagi_core::infrastructure::ipc::is_workspace_mismatch(error).then_some(error)
    }
}

/// Overview の session command port を workspace 起動ごとに新しく作る合成側 factory。
///
/// screen graph（Welcome→Open / Recent）は 1 ループで複数の workspace を順に開くため、
/// daemon の revision state を持ち越さないよう port を都度生成する。
#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=production_session_completion_contract
fn request_lifecycle_snapshot() -> Result<LifecycleSnapshot, LifecycleRequestError> {
    let mut client =
        crate::runtime::daemon::policy_client(usagi_core::usecase::client::ClientPolicy::tui())
            .map_err(LifecycleRequestError::Connect)?;
    match client
        .request(DaemonRequest::Session {
            action: SessionAction::List,
            operation_id: usagi_core::domain::id::OperationId::new().to_string(),
            payload: serde_json::json!({}),
        })
        .map_err(LifecycleRequestError::Request)?
    {
        DaemonReply::Ok(value) => lifecycle_snapshot(&value).map_err(LifecycleRequestError::Decode),
        DaemonReply::Accepted { .. } => Err(LifecycleRequestError::Decode(
            "daemon returned an invalid lifecycle snapshot response".to_owned(),
        )),
    }
}

/// The error [`WorkspaceLoader::open`] reports when the daemon cannot describe
/// the workspace being opened.
///
/// A workspace-fence refusal becomes [`std::io::ErrorKind::PermissionDenied`],
/// which is the entry screen's contract for "this daemon does not serve that
/// workspace": it stays on the current screen and shows this message instead of
/// tearing the TUI down. The message keeps the daemon's own wording — it names
/// the workspace that *is* served — and adds the one recovery step, because a
/// data directory has one daemon and that daemon has one workspace.
fn workspace_open_error(error: LifecycleRequestError, opened: &Path) -> std::io::Error {
    if let Some(refusal) = error.workspace_refusal() {
        return std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "cannot open {opened}: {message}. Stop it with `usagi daemon stop`, then start usagi in {opened}.",
                opened = opened.display(),
                message = refusal.message,
            ),
        );
    }
    io_error(error.reason())
}

/// Render only the user-actionable daemon reason in the TUI.  Error codes and
/// transport variant labels remain useful to diagnostics but add no context to
/// an interactive failure notice.
fn daemon_error_reason(error: ClientError) -> String {
    match error {
        ClientError::Protocol(error) => error.message,
        ClientError::Unavailable(message) | ClientError::Lifecycle(message) => message,
        ClientError::RolloverRequired(trigger) => format!(
            "daemon build rollover is required (operation {}); the current daemon remains running",
            trigger.operation_id.0
        ),
        ClientError::BuildIdentityUnavailable => {
            "exact daemon build identity is unavailable; the current daemon remains running"
                .to_owned()
        }
        ClientError::BootstrapContended => {
            "another usagi process is establishing the daemon connection; retrying".to_owned()
        }
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
    workspace: Option<WorkspaceSettingsStore>,
}

impl PersistentSettingsPort {
    #[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=global_modal_mode_survives_a_new_tui_settings_port
    fn open() -> std::io::Result<Self> {
        Ok(Self {
            storage: Storage::new(crate::runtime::daemon::prepare_private_data_dir()?),
            workspace: None,
        })
    }
}

impl SettingsPort for PersistentSettingsPort {
    fn select_workspace(&mut self, workspace_root: &Path) -> std::io::Result<()> {
        self.workspace = Some(WorkspaceSettingsStore::new(workspace_root));
        Ok(())
    }

    fn read(&mut self, scope: SettingsScope) -> std::io::Result<Settings> {
        Ok(match scope {
            SettingsScope::Global => self.storage.load_settings().map_err(io_error)?,
            SettingsScope::Workspace => {
                let global = self.storage.load_settings().map_err(io_error)?;
                let local = self
                    .workspace
                    .as_ref()
                    .map(WorkspaceSettingsStore::load)
                    .transpose()
                    .map_err(io_error)?
                    .unwrap_or_default();
                global.with_local(&local)
            }
        })
    }

    fn save(&mut self, scope: SettingsScope, settings: &Settings) -> std::io::Result<()> {
        match scope {
            SettingsScope::Global => {
                let _lock = self.storage.lock().map_err(io_error)?;
                self.storage.save_settings(settings).map_err(io_error)?;
            }
            SettingsScope::Workspace => {
                let workspace = self
                    .workspace
                    .as_ref()
                    .ok_or_else(|| io_error("workspace settings require an opened workspace"))?;
                let _lock = workspace.lock().map_err(io_error)?;
                workspace
                    .save(&LocalSettings::from(settings))
                    .map_err(io_error)?;
            }
        }
        Ok(())
    }
}

#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=real_pty_entry_resize_quit_and_reattach_restore_terminal
impl Terminal for CrosstermTerminal {
    fn size(&mut self) -> std::io::Result<(usize, usize)> {
        let (cols, rows) = terminal::size()?;
        Ok((rows as usize, cols as usize))
    }

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

    fn wait(&mut self, duration: Duration) -> std::io::Result<()> {
        std::thread::sleep(duration);
        Ok(())
    }

    /// Wait out one animation frame on the input pump instead of sleeping, so a
    /// key pressed during the startup splash is observed instead of queued behind
    /// a `thread::sleep` (#556). Wake-up ticks are the pump's own cadence and are
    /// swallowed here; only real input and a resize reach the caller.
    fn wait_for_key(&mut self, duration: Duration) -> std::io::Result<Option<Key>> {
        let deadline = Instant::now() + duration;
        loop {
            if Instant::now() >= deadline {
                return Ok(None);
            }
            match self.input.next(self.input_started.elapsed())? {
                RuntimeEvent::Input(input) => {
                    let now = self.input_started.elapsed();
                    if let Some(key) = classify_terminal_input(&mut self.live_input, now, &input) {
                        return Ok(Some(key));
                    }
                }
                RuntimeEvent::Resize { .. } => {
                    self.renderer.reset_surface();
                    return Ok(Some(Key::Resize));
                }
                RuntimeEvent::Backend(()) | RuntimeEvent::Tick => {}
            }
        }
    }

    fn read_key(&mut self) -> std::io::Result<Key> {
        loop {
            match self.input.next(self.input_started.elapsed())? {
                RuntimeEvent::Input(input) => {
                    let now = self.input_started.elapsed();
                    if let Some(key) = classify_terminal_input(&mut self.live_input, now, &input) {
                        return Ok(key);
                    }
                }
                // A resize is reported as itself, not as a generic wake-up. The
                // frame loop used to treat the two identically and fired the
                // decision + session RPCs for both, so dragging a window edge
                // produced one daemon round trip per resize event (#551).
                RuntimeEvent::Resize { .. } => {
                    self.renderer.reset_surface();
                    return Ok(Key::Resize);
                }
                // Tick wakes the TUI while a background session command owns
                // the daemon port, so the pending skeleton can redraw.
                RuntimeEvent::Backend(()) | RuntimeEvent::Tick => return Ok(Key::Other),
            }
        }
    }

    fn copy_text(&mut self, text: &str) -> Result<(), String> {
        use usagi_tui::usecase::application::terminal_selection::ClipboardPort;
        self.clipboard.write_text(text)
    }
}

/// Apply the live-input ordering policy before projecting terminal input into the
/// management [`Key`] vocabulary. `LiveInputClassifier` is the sole owner of
/// leader precedence; this adapter only translates its resolved output.
fn classify_terminal_input(
    classifier: &mut LiveInputClassifier,
    now: Duration,
    input: &LiveInput,
) -> Option<Key> {
    match classifier.classify(now, input.clone()) {
        LiveInputOutput::Action(action) => Some(Key::Live(action)),
        LiveInputOutput::GlobalControl(control) => Some(match control {
            GlobalControlChord::CtrlC => terminal_copy_key(input).unwrap_or(Key::Quit),
            GlobalControlChord::CtrlQ => Key::CtrlQ,
            GlobalControlChord::CtrlD => Key::CtrlD,
        }),
        LiveInputOutput::Swallowed => None,
        LiveInputOutput::Passthrough(bytes) => match input {
            LiveInput::Pointer(pointer) => Some(Key::Pointer(*pointer)),
            LiveInput::Mouse { column, row } => Some(Key::Click {
                column: *column,
                row: *row,
            }),
            _ => terminal_copy_key(input).or_else(|| Some(passthrough_key(input, bytes))),
        },
    }
}

/// Maps each supported platform's terminal copy chord to a selection-aware
/// request. Windows retains Ctrl-C as a PTY SIGINT when there is no selection.
fn terminal_copy_key(input: &LiveInput) -> Option<Key> {
    let LiveInput::Key(key) = input else {
        return None;
    };
    let only = |control, shift, super_| {
        key.modifiers.control == control
            && key.modifiers.shift == shift
            && key.modifiers.super_ == super_
            && !key.modifiers.alt
            && !key.modifiers.hyper
            && !key.modifiers.meta
    };
    #[cfg(target_os = "macos")]
    let matches_copy = matches!(key.code, KeyCode::Char('c')) && only(false, false, true);
    #[cfg(target_os = "windows")]
    let matches_copy = matches!(key.code, KeyCode::Char('c')) && only(true, false, false);
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let matches_copy = matches!(key.code, KeyCode::Char('c')) && only(true, true, false);

    #[cfg(target_os = "windows")]
    let fallback = vec![3];
    #[cfg(not(target_os = "windows"))]
    let fallback = Vec::new();

    matches_copy.then_some(Key::TerminalCopy { fallback })
}

/// Map a non-prefix live input to the management `Key` vocabulary. The classifier
/// has already reserved the `Ctrl-O` prefix, so this preserves the prior mapping
/// for every other key and text/paste payload.
#[coverage(off)] // coverage: reason=generic_monomorphization owner=tui expires=2027-01-31 tests=production_input_classifier_contract
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
        // A bracketed paste is delivered as one block. Carry the text so the
        // focused live pane wraps it in bracketed-paste markers before it reaches
        // the PTY (agents insert it instead of submitting each embedded newline)
        // and a management text input inserts it verbatim.
        LiveInput::Paste(_) => {
            return Key::Paste(String::from_utf8_lossy(&bytes).into_owned());
        }
        LiveInput::Raw(_) | LiveInput::Text(_) => {
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

fn io_error(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

pub(crate) fn resolve_workspace_path(path: &Path) -> std::io::Result<PathBuf> {
    let resolved = std::fs::canonicalize(path)?;
    validate_workspace_directory(&resolved)?;
    Ok(resolved)
}

#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=workspace_directory_validation_projects_metadata_errors
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
    #[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=direct_workspace_production_composition_contract
    fn open_default() -> std::io::Result<Self> {
        Ok(Self {
            storage: Storage::open_default().map_err(io_error)?,
        })
    }

    fn initialize_workspace_settings(&self, path: &Path) -> std::io::Result<()> {
        let defaults = self.storage.load_settings().map_err(io_error)?;
        WorkspaceSettingsStore::new(path)
            .initialize(&LocalSettings::from(&defaults))
            .map_err(io_error)
    }
}

#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=production_screen_graph_workspace_loader_contract
impl WorkspaceLoader for FsWorkspaceLoader {
    fn open(&mut self, path: &Path) -> std::io::Result<WorkspaceSnapshot> {
        validate_workspace_directory(path)?;
        // Declare the workspace being opened before anything else touches the
        // daemon: a daemon that serves another workspace then refuses this
        // connection instead of answering with its own sessions, and a cold start
        // binds the workspace being opened rather than this process's directory.
        // The refusal lands before any registry write or recent-list update, so a
        // workspace that cannot be shown is not recorded as opened either.
        let opened = crate::runtime::daemon::declare_opened_workspace(path)?;
        let lifecycle =
            request_lifecycle_snapshot().map_err(|error| workspace_open_error(error, &opened))?;
        let workspace =
            workspace_usecase::open(&self.storage, path, Utc::now()).map_err(io_error)?;
        // New workspaces copy Global's Agent / Issue / Memory defaults. For a
        // pre-existing registration from before workspace settings existed,
        // the first open performs the same one-time initialization. The store
        // never overwrites an existing workspace file.
        self.initialize_workspace_settings(&workspace.path)?;
        let mut state = load_workspace_state(&workspace.path)?;
        let workspace_id = lifecycle.workspace_id;
        // Identities align with the listed rows (`project` lists the same set),
        // so a `Failed` row shows on the first frame with a removable action.
        let session_ids = lifecycle
            .listed_sessions()
            .map(|session| session.session_id)
            .collect();
        let session_lifecycles = lifecycle.session_lifecycles();
        state.sessions = lifecycle.project(&workspace, &state.sessions);
        Ok(WorkspaceSnapshot::with_runtime_projection(
            workspace,
            state,
            workspace_id,
            session_ids,
            lifecycle.agent_resumes,
            session_lifecycles,
        ))
    }

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

    fn unregister(&mut self, paths: &[PathBuf]) -> std::io::Result<Vec<PathBuf>> {
        Ok(workspace_usecase::remove(&self.storage, paths)
            .map_err(io_error)?
            .into_iter()
            .map(|workspace| workspace.path)
            .collect())
    }

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

#[coverage(off)] // coverage: reason=generic_monomorphization owner=tui expires=2027-01-31 tests=welcome_start_loads_or_projects_storage_errors
fn load_screen_graph_data(
    storage: &Storage,
    start: Start,
) -> std::io::Result<(Vec<Workspace>, Vec<Recent>)> {
    match start {
        Start::Welcome => load_welcome_screen_data(storage),
        Start::Config => Ok((
            storage.load_workspaces().unwrap_or_default(),
            workspace_usecase::recent(storage).unwrap_or_default(),
        )),
    }
}

#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=welcome_start_loads_or_projects_storage_errors
fn load_welcome_screen_data(storage: &Storage) -> std::io::Result<(Vec<Workspace>, Vec<Recent>)> {
    Ok((
        storage.load_workspaces().map_err(io_error)?,
        workspace_usecase::recent(storage).map_err(io_error)?,
    ))
}

#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=real_pty_entry_resize_quit_and_reattach_restore_terminal
fn run_in_terminal(
    run: impl FnOnce(&mut CrosstermTerminal) -> std::io::Result<Exit>,
) -> std::io::Result<Exit> {
    enable_raw_mode()?;
    let mut setup = std::io::stdout();
    if let Err(error) = execute!(
        setup,
        EnterAlternateScreen,
        EnableMouseCapture,
        // Capture pastes as a single `Event::Paste` so a multi-line paste reaches
        // the focused pane as one block instead of a key stream whose embedded
        // Enters each submit a line to the agent (see `passthrough_key`).
        EnableBracketedPaste,
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
            CrosstermSource::default(),
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
        DisableBracketedPaste,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
    result
}

/// Keeps the daemon metrics observer alive for exactly one interactive TUI
/// lifetime.  A fresh connection-local subscription is created on every TUI
/// launch; orderly teardown explicitly unregisters it.
///
/// The subscription is display-only diagnostics, so it uses the observation
/// client: it declares no workspace and never starts a daemon. An entry screen
/// has not chosen a workspace yet, and cold-starting one here would bind the
/// daemon to the launch directory and make every later open refuse. For the same
/// reason a missing or refusing daemon only means "no metrics" — never a TUI that
/// will not start.
#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=metrics_hook_registration_contract
fn run_with_metrics_hook(run: impl FnOnce() -> std::io::Result<Exit>) -> std::io::Result<Exit> {
    let mut observer = crate::runtime::daemon::observation_client(ClientPolicy::tui())
        .ok()
        .and_then(|mut client| {
            let mut hook = MetricsHook::default();
            hook.connect(&mut client).ok().map(|()| (hook, client))
        });
    let result = run();
    if let Some((hook, client)) = observer.as_mut() {
        let _ = hook.shutdown(client);
    }
    result
}

#[coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=screen_graph_production_port_harness
fn launch_screen_graph(out: &mut dyn Write, start: Start) -> std::io::Result<()> {
    let now = Utc::now();
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        let storage = Storage::open_default().map_err(io_error)?;
        let (workspaces, recent) = load_screen_graph_data(&storage, start)?;
        let mut loader = FsWorkspaceLoader { storage };
        let mut settings = PersistentSettingsPort::open()?;
        let mut backend_factory = ProductionBackendFactory;
        let mut splash = presentation::StartupSplash::new();
        run_with_metrics_hook(|| {
            run_in_terminal(|terminal| {
                if start == Start::Welcome {
                    splash.play(terminal)?;
                }
                // The graph resolves "leave this workspace" into its own Welcome
                // screen, so it only returns when the process is ending. The
                // splash therefore plays once per launch, not once per Welcome.
                presentation::run_screen_graph_with_backend(
                    terminal,
                    workspaces,
                    recent,
                    now,
                    start,
                    &mut loader,
                    &mut settings,
                    &mut backend_factory,
                    available_agent_models(),
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

/// Probe every model provider in the shared vocabulary, so the Config screen and
/// the Closeup `agent -m` picker offer exactly the CLIs installed here.
fn available_agent_models() -> AvailableAgentModels {
    AvailableAgentModels::new(
        usagi_core::domain::settings::DefaultModel::ALL
            .into_iter()
            .filter(|model| cli_is_available(model.command())),
    )
}

fn cli_is_available(program: &str) -> bool {
    Command::new(program).arg("--version").output().is_ok()
}

/// Composition adapter for the daemon-owned PR snapshot. It deliberately has no
/// local scanner or state fallback: a failed request remains a safe TUI message
/// and a later snapshot retries convergence.
struct DaemonPrSnapshotPort;

#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=production_backend_factory_effect_matrix
impl PrSnapshotPort for DaemonPrSnapshotPort {
    fn snapshot(
        &mut self,
        session_id: usagi_core::domain::id::SessionId,
    ) -> Result<usagi_core::usecase::client::PrSnapshot, String> {
        let mut client = crate::runtime::daemon::policy_client(ClientPolicy::tui())
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

#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=production_backend_factory_effect_matrix
impl BrowserOpener for PlatformBrowserOpener {
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

#[coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=direct_workspace_production_composition_contract
fn launch_workspace(out: &mut dyn Write, path: &Path) -> std::io::Result<()> {
    let mut loader = FsWorkspaceLoader::open_default()?;
    let snapshot = loader.open(path)?;
    let mut settings = PersistentSettingsPort::open()?;
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        let mut backend_factory = ProductionBackendFactory;
        run_with_metrics_hook(|| {
            run_in_terminal(|terminal| {
                // A direct workspace entry has no Welcome behind it, so leaving
                // continues into the entry screens in this same process rather
                // than ending it: `usagi <path>` switches workspaces too (#556).
                // The workspace's ports are already dropped by the time the
                // controller returns, so the switcher starts with no connection
                // to the workspace that was left.
                match presentation::run_workspace_controller_with_backend_and_config(
                    terminal,
                    snapshot,
                    &mut backend_factory,
                    &mut settings,
                    available_agent_models(),
                )? {
                    Exit::Quit => Ok(Exit::Quit),
                    Exit::Welcome => {
                        // Read Recent now, not at launch: the workspace that was
                        // just left is the most recent one and belongs at the top.
                        let (workspaces, recent) =
                            load_screen_graph_data(&loader.storage, Start::Welcome)?;
                        presentation::run_screen_graph_with_backend(
                            terminal,
                            workspaces,
                            recent,
                            Utc::now(),
                            Start::Welcome,
                            &mut loader,
                            &mut settings,
                            &mut backend_factory,
                            available_agent_models(),
                        )
                    }
                }
            })
        })?;
    } else {
        for line in presentation::render_home_snapshot(0, 0, &snapshot) {
            writeln!(out, "{line}")?;
        }
    }
    Ok(())
}

/// Opening a workspace is the only entry that needs a daemon before the screen
/// appears, and the declaration has to be in place first so that a cold start
/// binds the workspace being opened.
///
/// Welcome / Open / New read local stores only. Probing daemon readiness there
/// would cold-start a daemon bound to whatever directory the TUI was launched
/// from, and that daemon would then refuse every workspace the switcher can open.
/// The workspace-bound connections those screens make when a workspace is chosen
/// carry their own declaration and bootstrap the daemon there.
#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=other_entries_route_to_their_banner_screens
fn with_daemon_ready(
    out: &mut dyn Write,
    info: &AppInfo,
    entry: &EntryScreen,
) -> std::io::Result<()> {
    if let EntryScreen::Workspace { path } = entry {
        // A path that cannot be declared is reported by `launch_workspace` as the
        // workspace error it is, not as an unavailable daemon.
        if crate::runtime::daemon::declare_opened_workspace(path).is_ok()
            && let Err(error) = crate::runtime::daemon::ensure_ready()
        {
            writeln!(std::io::stderr(), "daemon unavailable: {error}")?;
            return Ok(());
        }
    }
    launch_ready(out, info, entry)
}

fn launch_ready(out: &mut dyn Write, info: &AppInfo, entry: &EntryScreen) -> std::io::Result<()> {
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

#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=other_entries_route_to_their_banner_screens
pub(crate) fn launch(
    out: &mut dyn Write,
    info: &AppInfo,
    entry: &EntryScreen,
) -> std::io::Result<()> {
    match with_daemon_ready(out, info, entry) {
        // Opening a workspace this daemon does not serve is a refusal to present,
        // not a crash: the message already names the workspace that is served and
        // the step that switches to the one that was asked for.
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            writeln!(std::io::stderr(), "{error}")
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    #![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use usagi_core::usecase::client::{ClientPolicy, TerminalLaneBudget};

    /// The lane clock counts whole milliseconds, so a deadline armed for `n` ms
    /// can elapse a fraction under `n`. Lane-budget assertions allow that much.
    const CLOCK_GRANULARITY_MS: u64 = 2;

    use super::{
        DaemonAgentCommandPort, DaemonDecisionCommandPort, DaemonRestoreConnectionPort, EnvScope,
        EnvironmentStorePort, FsWorkspaceLoader, Geometry, LANE_COLD_START_BUDGET, LaneConnection,
        LifecycleRequestError, LifecycleSnapshot, PersistentSettingsPort, ProductionBackendFactory,
        RepoEnvironmentStore, SettingsEnvironmentStore, Start, StoreTarget, TerminalAttachScreen,
        TerminalChunk, TerminalError, TerminalInputOutcome, TerminalSnapshotMode,
        TerminalSubscription, agent_inventory_request, classify_terminal_input,
        created_session_hook, daemon_error_reason, decision_cadence, decode_agent_admission,
        decode_attach_screen, decode_exact_agent_resume, decode_terminal_input_ack,
        decode_terminal_inventory, decode_terminal_poll, exact_agent_resume_request,
        lifecycle_snapshot, load_screen_graph_data, load_workspace_state, map_terminal_error,
        metrics_cadence, passthrough_key, probe_path, provider_resume_projection, session_cadence,
        session_snapshot_result, terminal_copy_key, terminal_inventory_matches_scope,
        validate_workspace_directory, workspace_open_error,
    };
    use crate::runtime::refresh_pump::{MAX_INTERVAL, MIN_INTERVAL};
    use crate::runtime::terminal_pump::TerminalPollPump;
    use chrono::Utc;

    /// The contract of Home's three background observation lanes (#551).
    ///
    /// Each lane's cadence must sit inside the pump's bounded window — that is
    /// what caps the idle request rate — and cold-start authority must belong to
    /// exactly one lane, so a missing daemon cannot be raced for by three
    /// resident threads.
    #[test]
    fn refresh_pump_lane_contract() {
        for cadence in [decision_cadence(), session_cadence(), metrics_cadence()] {
            assert!(cadence.interval >= MIN_INTERVAL);
            assert!(cadence.interval <= MAX_INTERVAL);
            assert!(cadence.backoff_base >= cadence.interval / 2);
            assert!(cadence.backoff_max >= cadence.backoff_base);
        }
        // A pending decision should surface sooner than a foreign lifecycle
        // change, and the metrics sample keeps the rate it always had.
        assert!(decision_cadence().interval < session_cadence().interval);
        assert_eq!(metrics_cadence().interval, Duration::from_secs(1));

        let observing = LaneConnection::observing();
        assert_eq!(observing.cold_start_budget, 0);
        assert!(observing.client.is_none());
        assert_eq!(
            LaneConnection::lifecycle().cold_start_budget,
            LANE_COLD_START_BUDGET
        );
    }
    use serde_json::json;
    use usagi_core::domain::agent::{ProviderResumeProjection, ProviderResumeReason};
    use usagi_core::domain::id::{
        AgentContinuationRef, DaemonGeneration, OperationId, SessionId, TerminalId, TerminalRef,
        WorkspaceId, WorktreeId,
    };
    use usagi_core::domain::note::Scratchpad;
    use usagi_core::domain::session::{SessionOrigin, SessionRecord};
    use usagi_core::domain::session_lifecycle::{ManagedSession, SessionLifecycle};
    use usagi_core::domain::settings::{LocalSettings, ModalSelectionMode, Settings, Theme};
    use usagi_core::domain::terminal_launch::TerminalLaunchScope;
    use usagi_core::domain::workspace::Workspace;
    use usagi_core::domain::workspace_state::WorkspaceState;
    use usagi_core::infrastructure::paths::project_data_dir;
    use usagi_core::infrastructure::store::settings::WorkspaceSettingsStore;
    use usagi_core::infrastructure::store::workspace::Storage;
    use usagi_core::usecase::client::ClientError;
    use usagi_core::usecase::settings::{SettingsPort, SettingsScope};
    use usagi_tui::presentation::views::workspace::ProjectedSession;
    use usagi_tui::presentation::workspace_runtime::WorkspaceRuntime;
    use usagi_tui::presentation::{
        ControllerBackendFactory, ControllerHost, ControllerHostAction, RestoreConnectionPort,
        WorkspaceSnapshot,
    };
    use usagi_tui::usecase::application::Key;
    use usagi_tui::usecase::application::controller::{
        BackendEvent, Effect, EntryEvent, EntryState, EntryWorkspace, EnvironmentEntry, NewEvent,
        NewForm, NewMode, NewState, Notice, Target, update_entry, update_new,
    };
    use usagi_tui::usecase::terminal_input::{
        KeyCode, KeyEvent, KeyEventKind, LiveInput, Modifiers, PointerEvent, PointerKind,
    };

    /// The workspace root the scripted daemon fixtures own, and the declaration
    /// their clients send so the handshake fence admits them (#548).
    const TEST_WORKSPACE_ROOT: &str = "/workspace/root";

    fn test_client_workspace() -> usagi_core::infrastructure::ipc::ClientWorkspace {
        usagi_core::infrastructure::ipc::ClientWorkspace::Bound {
            root: TEST_WORKSPACE_ROOT.to_owned(),
        }
    }

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

    /// The serialized checkpoint a daemon at `rows`×`cols` produces after
    /// receiving `bytes`. Mirrors the daemon's grid authority in one value.
    fn screen_checkpoint_value(bytes: &[u8], rows: usize, cols: usize) -> serde_json::Value {
        use usagi_core::usecase::vt_screen::VtScreen;

        let mut screen = VtScreen::new(rows, cols);
        screen.advance(bytes);
        serde_json::to_value(screen.checkpoint()).expect("checkpoint serializes")
    }

    #[cfg(unix)]
    fn terminal_input_port(
        replies: Vec<(
            usagi_core::infrastructure::ipc::ResponseOutcome,
            serde_json::Value,
        )>,
    ) -> (
        DaemonAgentCommandPort,
        std::thread::JoinHandle<Vec<serde_json::Value>>,
    ) {
        terminal_input_port_with(replies, |_| {})
    }

    /// Same scripted daemon, with `adjust` narrowing what the server advertises
    /// so the client's snapshot negotiation can be exercised against an older
    /// daemon (absent capability, or a lower `max_revision`).
    #[cfg(unix)]
    fn terminal_input_port_with(
        replies: Vec<(
            usagi_core::infrastructure::ipc::ResponseOutcome,
            serde_json::Value,
        )>,
        adjust: impl FnOnce(&mut usagi_core::infrastructure::ipc::ServerProtocol),
    ) -> (
        DaemonAgentCommandPort,
        std::thread::JoinHandle<Vec<serde_json::Value>>,
    ) {
        let (client, server) = scripted_terminal_connection(replies, adjust);
        (
            DaemonAgentCommandPort {
                terminal: Some(client),
                poll: None,
                pump: TerminalPollPump::spawn(|_| Ok(Vec::new())),
                inventory: None,
                terminal_epoch: 1,
                attachments: Vec::new(),
                restore_connection: None,
                terminal_watch_cancelled: None,
            },
            server,
        )
    }

    /// One scripted daemon connection: it answers `replies` in order, then closes
    /// the socket. A request beyond the script therefore reads EOF, which is the
    /// transport failure a stopped or restarted daemon produces mid-stream. The
    /// joined handle yields the request bodies the connection actually received.
    #[cfg(unix)]
    fn scripted_terminal_connection(
        replies: Vec<(
            usagi_core::infrastructure::ipc::ResponseOutcome,
            serde_json::Value,
        )>,
        adjust: impl FnOnce(&mut usagi_core::infrastructure::ipc::ServerProtocol),
    ) -> (
        super::LaneClient,
        std::thread::JoinHandle<Vec<serde_json::Value>>,
    ) {
        use std::os::unix::net::UnixStream;

        use usagi_core::infrastructure::ipc::{
            BuildIdentity, DaemonGeneration, Envelope, EnvelopeKind, read_json_frame,
            write_json_frame,
        };
        use usagi_core::usecase::client::{ClientPolicy, IpcClient};
        use usagi_daemon::presentation::ipc::{handshake, server_protocol};

        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let build = BuildIdentity {
            version: "test".to_owned(),
            commit: "test".to_owned(),
            target: "test".to_owned(),
            artifact: "test-artifact".to_owned(),
        };
        let mut protocol = server_protocol(
            DaemonGeneration("input-ack-test".to_owned()),
            "input-ack-connection".to_owned(),
            build.clone(),
            usagi_core::domain::daemon::DaemonRecord::identified(2, "test-process"),
            TEST_WORKSPACE_ROOT.to_owned(),
        );
        adjust(&mut protocol);
        let server = std::thread::spawn(move || {
            let mut reader = server_stream.try_clone().unwrap();
            let mut writer = server_stream;
            let hello = handshake(&mut reader, &mut writer, &protocol)
                .unwrap()
                .unwrap();
            let mut requests = Vec::with_capacity(replies.len());
            for (outcome, body) in replies {
                let request =
                    read_json_frame::<Envelope>(&mut reader, hello.limits.max_frame_bytes as usize)
                        .unwrap()
                        .expect("scripted terminal input request");
                let EnvelopeKind::Request {
                    request_id,
                    body: request_body,
                    ..
                } = request.kind
                else {
                    panic!("terminal client sent a non-request envelope");
                };
                requests.push(request_body);
                write_json_frame(
                    &mut writer,
                    &Envelope {
                        protocol: hello.protocol,
                        daemon_generation: hello.daemon_generation.clone(),
                        kind: EnvelopeKind::Response {
                            request_id,
                            outcome,
                            body,
                        },
                    },
                    hello.limits.max_frame_bytes as usize,
                )
                .unwrap();
            }
            requests
        });
        // The production lane transport, so every scripted exchange is bounded
        // by the same deadline stream the TUI actually runs over.
        let client = IpcClient::connect(
            crate::runtime::daemon::deadline_transport(
                crate::runtime::daemon::SystemClock::new(),
                client_stream,
                ClientPolicy::tui().timeout_ms,
            ),
            "input-ack-client".to_owned(),
            "input-ack-nonce".to_owned(),
            ClientPolicy::tui(),
            build,
            test_client_workspace(),
        )
        .unwrap();
        (client, server)
    }

    /// A subscription on the scripted fixture's connection epoch.
    #[cfg(unix)]
    fn subscription(id: u64) -> TerminalSubscription {
        TerminalSubscription { id, epoch: 1 }
    }

    #[cfg(unix)]
    fn input_terminal_ref() -> TerminalRef {
        serde_json::from_value(json!({
            "daemon_generation": "00000000-0000-4000-8000-000000000001",
            "terminal_id": "00000000-0000-4000-8000-000000000002",
            "workspace_id": "00000000-0000-4000-8000-000000000003",
            "session_id": null,
            "worktree_id": "00000000-0000-4000-8000-000000000004"
        }))
        .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn production_terminal_input_decodes_every_known_ack_outcome() {
        use usagi_core::infrastructure::ipc::ResponseOutcome;
        use usagi_tui::presentation::AgentCommandPort;

        let (mut port, server) = terminal_input_port(vec![
            (ResponseOutcome::Ok, json!({ "ack": "Written" })),
            (
                ResponseOutcome::Ok,
                json!({ "ack": { "Cached": { "Cached": "Written" } } }),
            ),
            (ResponseOutcome::Ok, json!({ "ack": "Failed" })),
            (
                ResponseOutcome::Ok,
                json!({ "ack": { "Cached": "Failed" } }),
            ),
            (
                ResponseOutcome::Ok,
                json!({ "ack": { "Ambiguous": { "applied_prefix": 2 } } }),
            ),
            (
                ResponseOutcome::Ok,
                json!({ "ack": { "Cached": { "Ambiguous": { "applied_prefix": 3 } } } }),
            ),
        ]);
        let terminal = input_terminal_ref();

        assert_eq!(
            port.input_terminal(&terminal, subscription(7), 0, OperationId::new(), b"x"),
            Ok(TerminalInputOutcome::Written)
        );
        assert_eq!(
            port.input_terminal(&terminal, subscription(7), 1, OperationId::new(), b"x"),
            Ok(TerminalInputOutcome::Written)
        );
        assert_eq!(
            port.input_terminal(&terminal, subscription(7), 2, OperationId::new(), b"x"),
            Ok(TerminalInputOutcome::Failed)
        );
        assert_eq!(
            port.input_terminal(&terminal, subscription(7), 3, OperationId::new(), b"x"),
            Ok(TerminalInputOutcome::Failed)
        );
        assert_eq!(
            port.input_terminal(&terminal, subscription(7), 4, OperationId::new(), b"abc"),
            Ok(TerminalInputOutcome::Ambiguous { applied_prefix: 2 })
        );
        assert_eq!(
            port.input_terminal(&terminal, subscription(7), 5, OperationId::new(), b"abc"),
            Ok(TerminalInputOutcome::Ambiguous { applied_prefix: 3 })
        );

        drop(port);
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn production_terminal_input_rejects_accepted_as_a_non_final_ack() {
        use usagi_core::infrastructure::ipc::ResponseOutcome;
        use usagi_tui::presentation::AgentCommandPort;

        let (mut port, server) = terminal_input_port(vec![(
            ResponseOutcome::Accepted {
                operation_id: usagi_core::infrastructure::ipc::OperationId(
                    OperationId::new().to_string(),
                ),
                operation_revision: 1,
            },
            json!({ "ack": "Written" }),
        )]);

        assert_eq!(
            port.input_terminal(
                &input_terminal_ref(),
                subscription(7),
                0,
                OperationId::new(),
                b"x"
            ),
            Err(TerminalError::InputEffectUnknown)
        );
        // The answer was fully received, so the stream stays consistent: the
        // shared connection — and every peer pane's subscription on it — survives
        // this pane's unknown input effect.
        assert!(port.terminal.is_some());
        assert_eq!(port.terminal_connection_epoch(), Some(1));
        drop(port);
        server.join().unwrap();
    }

    /// The production wire half of #519: the adapter carries the producer's
    /// operation identity on the input, resolves it with a read-only query, and
    /// never turns an unreadable or absent record into a success.
    #[cfg(unix)]
    #[test]
    fn production_terminal_input_carries_and_resolves_a_durable_operation() {
        use usagi_core::infrastructure::ipc::ResponseOutcome;
        use usagi_core::usecase::client::{DaemonRequest, TerminalAction, TerminalRequest};
        use usagi_tui::presentation::AgentCommandPort;
        use usagi_tui::usecase::application::terminal_session::TerminalInputResolution;

        let operation = OperationId::new();
        let (mut port, server) = terminal_input_port(vec![
            (ResponseOutcome::Ok, json!({ "ack": "Written" })),
            (
                ResponseOutcome::Ok,
                json!({ "outcome": "final", "ack": { "Cached": "Written" } }),
            ),
            (ResponseOutcome::Ok, json!({ "outcome": "unknown" })),
            (
                ResponseOutcome::Ok,
                json!({ "outcome": "final", "ack": { "Ambiguous": { "applied_prefix": 9 } } }),
            ),
            (ResponseOutcome::Ok, json!({ "outcome": "sideways" })),
        ]);
        let terminal = input_terminal_ref();

        assert_eq!(
            port.input_terminal(&terminal, subscription(7), 0, operation, b"ab"),
            Ok(TerminalInputOutcome::Written)
        );
        // A cached final normalizes to the outcome it wraps.
        assert_eq!(
            port.terminal_input_outcome(&terminal, operation, 2),
            Ok(TerminalInputResolution::Final(
                TerminalInputOutcome::Written
            ))
        );
        // No record: typed uncertainty, not an error and not a success.
        assert_eq!(
            port.terminal_input_outcome(&terminal, operation, 2),
            Ok(TerminalInputResolution::Unknown)
        );
        // An applied prefix beyond the input it belongs to is fail-closed.
        assert_eq!(
            port.terminal_input_outcome(&terminal, operation, 2),
            Err(TerminalError::InputEffectUnknown)
        );
        // An outcome vocabulary this build cannot read is not "nothing happened".
        assert_eq!(
            port.terminal_input_outcome(&terminal, operation, 2),
            Err(TerminalError::InputEffectUnknown)
        );
        // Every answer was fully received, so the shared connection is intact.
        assert!(port.terminal.is_some());
        assert_eq!(port.terminal_connection_epoch(), Some(1));
        drop(port);

        let requests = server.join().unwrap();
        let sent: Vec<(TerminalAction, TerminalRequest)> = requests
            .into_iter()
            .filter_map(|body| serde_json::from_value::<DaemonRequest>(body).ok())
            .filter_map(|request| match request {
                DaemonRequest::Terminal { action, payload } => {
                    serde_json::from_value::<TerminalRequest>(payload)
                        .ok()
                        .map(|request| (action, request))
                }
                _ => None,
            })
            .collect();
        assert!(matches!(
            &sent[0],
            (
                TerminalAction::Input,
                TerminalRequest::Input {
                    input_operation: Some(sent),
                    input_seq: 0,
                    ..
                },
            ) if *sent == operation
        ));
        // Each resolution is a read-only query naming the same operation; the
        // bytes are never sent again.
        assert!(sent[1..].iter().all(|entry| matches!(
            entry,
            (
                TerminalAction::InputOutcome,
                TerminalRequest::InputOutcome { input_operation, .. },
            ) if *input_operation == operation
        )));
    }

    /// A daemon without the durable ledger capability is treated as legacy: no
    /// operation identity is put on the wire, and resolution answers unknown
    /// without asking, so the client keeps its uncertainty instead of resending.
    #[cfg(unix)]
    #[test]
    fn a_daemon_without_the_input_operation_capability_fails_closed_to_legacy() {
        use usagi_core::infrastructure::ipc::{
            ResponseOutcome, TERMINAL_INPUT_OPERATION_CAPABILITY,
        };
        use usagi_core::usecase::client::{DaemonRequest, TerminalRequest};
        use usagi_tui::presentation::AgentCommandPort;
        use usagi_tui::usecase::application::terminal_session::TerminalInputResolution;

        let (mut port, server) = terminal_input_port_with(
            vec![(ResponseOutcome::Ok, json!({ "ack": "Written" }))],
            |protocol| {
                protocol
                    .capabilities
                    .retain(|capability| capability != TERMINAL_INPUT_OPERATION_CAPABILITY);
            },
        );
        let terminal = input_terminal_ref();
        let operation = OperationId::new();
        assert_eq!(
            port.input_terminal(&terminal, subscription(7), 0, operation, b"ab"),
            Ok(TerminalInputOutcome::Written)
        );
        assert_eq!(
            port.terminal_input_outcome(&terminal, operation, 2),
            Ok(TerminalInputResolution::Unknown)
        );
        drop(port);

        let requests = server.join().unwrap();
        let sent: Vec<TerminalRequest> = requests
            .into_iter()
            .filter_map(|body| serde_json::from_value::<DaemonRequest>(body).ok())
            .filter_map(|request| match request {
                DaemonRequest::Terminal { payload, .. } => {
                    serde_json::from_value::<TerminalRequest>(payload).ok()
                }
                _ => None,
            })
            .collect();
        // Exactly one request: the input, without an operation identity. The
        // resolution never reached the wire.
        assert!(matches!(
            sent.as_slice(),
            [TerminalRequest::Input {
                input_operation: None,
                ..
            }]
        ));
    }

    #[cfg(unix)]
    #[test]
    fn production_terminal_input_protocol_side_effect_controls_unknown_feedback() {
        use usagi_core::infrastructure::ipc::{
            ErrorCode, ProtocolError, ResponseOutcome, SideEffect,
        };
        use usagi_tui::presentation::AgentCommandPort;

        for (side_effect, expected) in [
            (SideEffect::None, TerminalError::Unavailable),
            (
                SideEffect::PartialOrUnknown,
                TerminalError::InputEffectUnknown,
            ),
            (SideEffect::Applied, TerminalError::InputEffectUnknown),
            (
                SideEffect::OperationAccepted,
                TerminalError::InputEffectUnknown,
            ),
        ] {
            let mut error = ProtocolError::new(ErrorCode::Unavailable, "scripted input failure");
            error.side_effect = side_effect;
            let (mut port, server) =
                terminal_input_port(vec![(ResponseOutcome::Error(error), json!(null))]);

            assert_eq!(
                port.input_terminal(
                    &input_terminal_ref(),
                    subscription(7),
                    0,
                    OperationId::new(),
                    b"x"
                ),
                Err(expected),
                "side effect {side_effect:?}"
            );
            // A protocol error is a finished request on a healthy socket, whatever
            // its side effect: the shared connection is kept so peer panes keep
            // their attachments.
            assert!(port.terminal.is_some(), "side effect {side_effect:?}");
            assert_eq!(port.terminal_connection_epoch(), Some(1));
            drop(port);
            server.join().unwrap();
        }
    }

    /// A second daemon-owned terminal, so two panes share one connection.
    #[cfg(unix)]
    fn peer_terminal_ref() -> TerminalRef {
        let mut terminal = input_terminal_ref();
        terminal.terminal_id = TerminalId::new();
        terminal
    }

    /// The terminal actions one scripted connection received, in order.
    #[cfg(unix)]
    fn terminal_actions(requests: Vec<serde_json::Value>) -> Vec<String> {
        use usagi_core::usecase::client::{DaemonRequest, TerminalRequest};

        requests
            .into_iter()
            .filter_map(|body| serde_json::from_value::<DaemonRequest>(body).ok())
            .filter_map(|request| match request {
                DaemonRequest::Terminal { action, payload } => {
                    serde_json::from_value::<TerminalRequest>(payload)
                        .ok()
                        .map(|request| (action, request))
                }
                _ => None,
            })
            .map(|(action, request)| match request {
                TerminalRequest::Input {
                    subscription,
                    input_seq,
                    ..
                } => format!("input#{input_seq}@{subscription}"),
                TerminalRequest::Attach { .. } => "attach".to_owned(),
                TerminalRequest::Detach { subscription, .. } => format!("detach@{subscription}"),
                _ => format!("{action:?}").to_lowercase(),
            })
            .collect()
    }

    /// The shared attach / input / detach connection is only replaced by a
    /// transport failure, and that replacement invalidates every pane's
    /// subscription at once.
    ///
    /// This is the production half of the cross-pane contract: a fully received
    /// `resync_required` for one pane keeps the socket its peers are attached on,
    /// while an EOF advances the epoch so no pane can spend a keystroke on an
    /// attachment the daemon released. Releasing a superseded subscription must
    /// leave neither the fresh attachment nor its output registration behind.
    #[cfg(unix)]
    #[test]
    #[allow(clippy::too_many_lines)] // Two scripted connections are one epoch contract.
    fn production_shared_connection_epoch_survives_protocol_errors_and_invalidates_on_eof() {
        use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError, ResponseOutcome};
        use usagi_tui::presentation::AgentCommandPort;

        let geometry = Geometry { cols: 20, rows: 3 };
        let attach_body = |subscription: u64| {
            json!({
                "subscription": subscription,
                "snapshot": {
                    "revision": 1,
                    "output_offset": 0,
                    "base_offset": 0,
                    "screen": screen_checkpoint_value(b"", 3, 20),
                    "exited": null
                }
            })
        };
        let agent = input_terminal_ref();
        let generic = peer_terminal_ref();

        // The first connection serves both panes, then stops answering: the ninth
        // request reads EOF, exactly as a restarted daemon leaves it.
        let (client, first) = scripted_terminal_connection(
            vec![
                (ResponseOutcome::Ok, attach_body(11)),
                (ResponseOutcome::Ok, attach_body(12)),
                (ResponseOutcome::Ok, json!({ "ack": "Written" })),
                (ResponseOutcome::Ok, json!({ "ack": "Written" })),
                (
                    ResponseOutcome::Error(ProtocolError::new(
                        ErrorCode::ResyncRequired,
                        "scripted pane-local resync",
                    )),
                    json!(null),
                ),
                (ResponseOutcome::Ok, attach_body(13)),
                (ResponseOutcome::Ok, json!({})),
                (ResponseOutcome::Ok, json!({ "ack": "Written" })),
            ],
            |_| {},
        );
        let mut port = DaemonAgentCommandPort {
            terminal: Some(client),
            poll: None,
            // The pump answers every registered terminal, so a lost registration
            // is observable as output that stops arriving.
            pump: TerminalPollPump::spawn(|fence| {
                Ok(vec![TerminalChunk {
                    start_offset: fence.after_offset,
                    end_offset: fence.after_offset + 2,
                    data: b"hi".to_vec(),
                }])
            }),
            inventory: None,
            terminal_epoch: 1,
            attachments: Vec::new(),
            restore_connection: None,
            terminal_watch_cancelled: None,
        };

        let agent_first = port.attach_terminal(&agent, geometry).unwrap();
        let generic_first = port.attach_terminal(&generic, geometry).unwrap();
        assert_eq!(agent_first.subscription, subscription(11));
        assert_eq!(generic_first.subscription, subscription(12));
        assert_eq!(
            port.input_terminal(
                &agent,
                agent_first.subscription,
                0,
                OperationId::new(),
                b"a"
            ),
            Ok(TerminalInputOutcome::Written)
        );
        assert_eq!(
            port.input_terminal(
                &generic,
                generic_first.subscription,
                0,
                OperationId::new(),
                b"b"
            ),
            Ok(TerminalInputOutcome::Written)
        );

        // One pane's fully received `resync_required` keeps the shared connection,
        // so the peer's subscription and the epoch are untouched.
        assert_eq!(
            port.attach_terminal(&agent, geometry),
            Err(TerminalError::ResyncRequired)
        );
        assert!(port.terminal.is_some());
        assert_eq!(port.terminal_connection_epoch(), Some(1));

        // That pane resyncs on the same connection and releases its superseded
        // subscription there — without revoking the attachment, or the output
        // registration, that replaced it.
        let agent_resync = port.attach_terminal(&agent, geometry).unwrap();
        assert_eq!(agent_resync.subscription, subscription(13));
        port.detach_terminal(&agent, agent_first.subscription);
        let mut streamed = Vec::new();
        for _ in 0..200 {
            streamed = port.poll_terminal(&agent, 0).unwrap();
            if !streamed.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !streamed.is_empty(),
            "the superseded release unregistered the fresh attachment's output"
        );
        // Same connection, so the daemon's ledger for this client continues.
        assert_eq!(
            port.input_terminal(
                &agent,
                agent_resync.subscription,
                1,
                OperationId::new(),
                b"a2"
            ),
            Ok(TerminalInputOutcome::Written)
        );

        // The next request reads EOF. The connection is dropped and the epoch
        // advances, which invalidates both panes' subscriptions at once.
        assert_eq!(
            port.attach_terminal(&generic, geometry),
            Err(TerminalError::Unavailable)
        );
        assert!(port.terminal.is_none());
        assert_eq!(port.terminal_connection_epoch(), Some(2));

        // A replaced subscription is refused before any connection is opened, so
        // the keystroke is definitely unwritten instead of rejected as unattached.
        assert_eq!(
            port.input_terminal(
                &generic,
                generic_first.subscription,
                1,
                OperationId::new(),
                b"lost"
            ),
            Err(TerminalError::Unavailable)
        );
        // Its release is local too: nothing is sent, and no connection is opened
        // to send it on.
        port.detach_terminal(&generic, generic_first.subscription);
        port.detach_terminal(&agent, agent_resync.subscription);
        assert!(port.terminal.is_none());
        assert_eq!(port.terminal_connection_epoch(), Some(2));

        // The reconnect the next attach performs, with the replacement connection
        // injected in place of the real socket.
        let (replacement, second) = scripted_terminal_connection(
            vec![
                (ResponseOutcome::Ok, attach_body(21)),
                (ResponseOutcome::Ok, json!({ "ack": "Written" })),
            ],
            |_| {},
        );
        port.terminal = Some(replacement);
        let generic_second = port.attach_terminal(&generic, geometry).unwrap();
        assert_eq!(
            generic_second.subscription,
            TerminalSubscription { id: 21, epoch: 2 }
        );
        // A new connection means a new daemon-side ledger, so the first input on
        // it starts at zero and is written once.
        assert_eq!(
            port.input_terminal(
                &generic,
                generic_second.subscription,
                0,
                OperationId::new(),
                b"k"
            ),
            Ok(TerminalInputOutcome::Written)
        );

        drop(port);
        // Nothing was sent for the replaced subscriptions, and on each connection
        // the attach precedes every input for that pane.
        assert_eq!(
            terminal_actions(first.join().unwrap()),
            vec![
                "attach",
                "attach",
                "input#0@11",
                "input#0@12",
                "attach",
                "attach",
                "detach@11",
                "input#1@13",
            ]
        );
        assert_eq!(
            terminal_actions(second.join().unwrap()),
            vec!["attach", "input#0@21"]
        );
    }

    /// A daemon that stops answering while keeping the socket open — the shape a
    /// hung daemon has, as distinct from the EOF a restarted one leaves.
    ///
    /// It answers `replies` in order, then reads one more request and answers
    /// nothing, holding the connection until the returned server is joined. The
    /// client can therefore only be freed by its own armed lane deadline, which is
    /// exactly what these fixtures need to prove.
    #[cfg(unix)]
    struct HungTerminalServer {
        release: mpsc::Sender<()>,
        handle: std::thread::JoinHandle<Vec<serde_json::Value>>,
    }

    #[cfg(unix)]
    impl HungTerminalServer {
        /// Releases the held connection and yields the request bodies it read.
        fn join(self) -> Vec<serde_json::Value> {
            let _ = self.release.send(());
            self.handle.join().unwrap()
        }
    }

    #[cfg(unix)]
    fn hung_terminal_connection(
        replies: Vec<(
            usagi_core::infrastructure::ipc::ResponseOutcome,
            serde_json::Value,
        )>,
    ) -> (super::LaneClient, HungTerminalServer) {
        use std::os::unix::net::UnixStream;

        use usagi_core::infrastructure::ipc::{
            BuildIdentity, DaemonGeneration, Envelope, EnvelopeKind, read_json_frame,
            write_json_frame,
        };
        use usagi_core::usecase::client::{ClientPolicy, IpcClient};
        use usagi_daemon::presentation::ipc::{handshake, server_protocol};

        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let build = BuildIdentity {
            version: "test".to_owned(),
            commit: "test".to_owned(),
            target: "test".to_owned(),
            artifact: "test-artifact".to_owned(),
        };
        let protocol = server_protocol(
            DaemonGeneration("hung-lane-test".to_owned()),
            "hung-lane-connection".to_owned(),
            build.clone(),
            usagi_core::domain::daemon::DaemonRecord::identified(2, "test-process"),
            TEST_WORKSPACE_ROOT.to_owned(),
        );
        let (release, released) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut reader = server_stream.try_clone().unwrap();
            let mut writer = server_stream;
            let hello = handshake(&mut reader, &mut writer, &protocol)
                .unwrap()
                .unwrap();
            let limit = hello.limits.max_frame_bytes as usize;
            let mut requests = Vec::new();
            let mut replies = replies.into_iter();
            while let Ok(Some(request)) = read_json_frame::<Envelope>(&mut reader, limit) {
                let EnvelopeKind::Request {
                    request_id,
                    body: request_body,
                    ..
                } = request.kind
                else {
                    panic!("terminal client sent a non-request envelope");
                };
                requests.push(request_body);
                let Some((outcome, body)) = replies.next() else {
                    // Read but never answered: the daemon may already have applied
                    // this request's effect, so the client must treat it as unknown
                    // rather than undelivered.
                    break;
                };
                write_json_frame(
                    &mut writer,
                    &Envelope {
                        protocol: hello.protocol,
                        daemon_generation: hello.daemon_generation.clone(),
                        kind: EnvelopeKind::Response {
                            request_id,
                            outcome,
                            body,
                        },
                    },
                    limit,
                )
                .unwrap();
            }
            // Hold the socket open so nothing but the client's own deadline can
            // end its wait.
            let _ = released.recv();
            requests
        });
        let client = IpcClient::connect(
            crate::runtime::daemon::deadline_transport(
                crate::runtime::daemon::SystemClock::new(),
                client_stream,
                ClientPolicy::tui().timeout_ms,
            ),
            "hung-lane-client".to_owned(),
            "hung-lane-nonce".to_owned(),
            ClientPolicy::tui(),
            build,
            test_client_workspace(),
        )
        .unwrap();
        (client, HungTerminalServer { release, handle })
    }

    #[cfg(unix)]
    fn lane_port(client: super::LaneClient) -> DaemonAgentCommandPort {
        DaemonAgentCommandPort {
            terminal: Some(client),
            poll: None,
            pump: TerminalPollPump::spawn(|_| Ok(Vec::new())),
            inventory: None,
            terminal_epoch: 1,
            attachments: Vec::new(),
            restore_connection: None,
            terminal_watch_cancelled: None,
        }
    }

    /// A hung daemon costs one keystroke its lane budget, not the UI.
    ///
    /// The attach/input lane used to be a plain socket with no deadline at all,
    /// so a daemon that read an `Input` and stopped answering froze the render
    /// thread forever (#553). It is now re-armed per request with
    /// [`TerminalLaneBudget`], and the overrun is handled as the ambiguity it is:
    /// the daemon may already have written the bytes to the PTY, so the keystroke
    /// is **never** replayed. It is resolved by the read-only `InputOutcome`
    /// query against the durable operation ledger (#519), which is the only lane
    /// action the retry table lets a fresh connection carry.
    #[cfg(unix)]
    #[test]
    fn a_hung_daemon_bounds_one_keystroke_and_resolves_it_by_ledger_query() {
        use usagi_core::infrastructure::ipc::ResponseOutcome;
        use usagi_tui::presentation::AgentCommandPort;
        use usagi_tui::usecase::application::terminal_session::TerminalInputResolution;

        let (client, hung) = hung_terminal_connection(Vec::new());
        let mut port = lane_port(client);
        let terminal = input_terminal_ref();
        let operation = OperationId::new();

        let started = Instant::now();
        assert_eq!(
            port.input_terminal(&terminal, subscription(7), 0, operation, b"ab"),
            Err(TerminalError::InputEffectUnknown)
        );
        let elapsed = started.elapsed();

        // The budget plus scheduler slack, and nowhere near the surface policy's
        // two seconds that the lane would otherwise have inherited.
        assert!(
            elapsed >= Duration::from_millis(TerminalLaneBudget::INPUT_MS - CLOCK_GRANULARITY_MS),
            "the deadline was actually reached: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(TerminalLaneBudget::INPUT_MS * 4),
            "one keystroke stayed within its lane budget: {elapsed:?}"
        );
        assert!(elapsed < Duration::from_millis(ClientPolicy::tui().timeout_ms));

        // The socket's position is unknown after the overrun, so the lane is
        // dropped and the epoch advances: every pane re-attaches (#523).
        assert!(port.terminal.is_none());
        assert_eq!(port.terminal_connection_epoch(), Some(2));

        // Exactly one write reached the daemon, and it was never repeated.
        assert_eq!(terminal_actions(hung.join()), vec!["input#0@7"]);

        // Resolution runs on the replacement connection, as a read-only query
        // naming the same operation. The recorded final is projected exactly as
        // the lost acknowledgement would have been.
        let (replacement, ledger) = scripted_terminal_connection(
            vec![(
                ResponseOutcome::Ok,
                json!({ "outcome": "final", "ack": "Written" }),
            )],
            |_| {},
        );
        port.terminal = Some(replacement);
        assert_eq!(
            port.terminal_input_outcome(&terminal, operation, 2),
            Ok(TerminalInputResolution::Final(
                TerminalInputOutcome::Written
            ))
        );
        drop(port);
        assert_eq!(
            terminal_actions(ledger.join().unwrap()),
            vec!["inputoutcome"]
        );
    }

    /// A keystroke whose request never reached the daemon is still reported as
    /// unknown, and still never resent.
    ///
    /// Once the write was attempted the client cannot distinguish "not sent" from
    /// "applied but the acknowledgement was lost", so it fails closed. What it
    /// must not do is decide the byte is undelivered and send it again: the
    /// resolution is the read-only ledger query. Here the daemon closed before
    /// reading anything, so the true effect count is zero — and the number of
    /// requests it received stays zero, proving no replay was attempted.
    #[cfg(unix)]
    #[test]
    fn a_request_that_never_reached_the_daemon_reports_unknown_without_resending() {
        use usagi_tui::presentation::AgentCommandPort;

        // Answers nothing and closes without reading: dispatch never happened.
        let (client, server) = scripted_terminal_connection(Vec::new(), |_| {});
        // The deadline transport still exposes the socket underneath, which is
        // what the restore watcher clones to peek for EOF; arming the lane must
        // not take that away.
        assert!(
            crate::runtime::daemon::lane_socket(&client)
                .try_clone()
                .is_ok()
        );
        let mut port = lane_port(client);

        assert_eq!(
            port.input_terminal(
                &input_terminal_ref(),
                subscription(7),
                0,
                OperationId::new(),
                b"ab"
            ),
            Err(TerminalError::InputEffectUnknown)
        );
        assert!(port.terminal.is_none());
        assert_eq!(port.terminal_connection_epoch(), Some(2));
        drop(port);
        assert!(terminal_actions(server.join().unwrap()).is_empty());
    }

    /// A lane deadline on one pane does not leave the others stranded: it drops
    /// the shared lane, and every pane comes back through the epoch path (#523).
    ///
    /// This is the cross-pane half of the deadline contract. The peer pane's
    /// subscription belongs to the epoch that just ended, so its next keystroke
    /// is refused *locally* — nothing is written, so the effect is definitively
    /// zero — and re-attaching on the fresh epoch restores both panes.
    #[cfg(unix)]
    #[test]
    fn a_lane_deadline_on_one_pane_reattaches_every_pane_through_the_epoch() {
        use usagi_core::infrastructure::ipc::ResponseOutcome;
        use usagi_tui::presentation::AgentCommandPort;

        let geometry = Geometry { cols: 20, rows: 3 };
        let attach_body = |subscription: u64| {
            json!({
                "subscription": subscription,
                "snapshot": {
                    "revision": 1,
                    "output_offset": 0,
                    "base_offset": 0,
                    "screen": screen_checkpoint_value(b"", 3, 20),
                    "exited": null
                }
            })
        };
        let agent = input_terminal_ref();
        let peer = peer_terminal_ref();

        // Both panes attach, then the daemon hangs on the agent pane's attach.
        let (client, hung) = hung_terminal_connection(vec![
            (ResponseOutcome::Ok, attach_body(11)),
            (ResponseOutcome::Ok, attach_body(12)),
        ]);
        let mut port = lane_port(client);
        let agent_first = port.attach_terminal(&agent, geometry).unwrap();
        let peer_first = port.attach_terminal(&peer, geometry).unwrap();
        assert_eq!(agent_first.subscription, subscription(11));
        assert_eq!(peer_first.subscription, subscription(12));

        let started = Instant::now();
        assert_eq!(
            port.attach_terminal(&agent, geometry),
            Err(TerminalError::Unavailable)
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed
                >= Duration::from_millis(TerminalLaneBudget::SNAPSHOT_MS - CLOCK_GRANULARITY_MS),
            "the snapshot budget was actually reached: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(TerminalLaneBudget::SNAPSHOT_MS * 4),
            "a tab switch stayed within its lane budget: {elapsed:?}"
        );
        assert!(port.terminal.is_none());
        assert_eq!(port.terminal_connection_epoch(), Some(2));
        assert_eq!(
            terminal_actions(hung.join()),
            vec!["attach", "attach", "attach"]
        );

        // The peer pane never asked for anything, yet its subscription is from the
        // epoch that ended. Its next keystroke is refused before a connection is
        // even opened, so the byte is definitely unwritten rather than rejected as
        // unattached.
        assert_eq!(
            port.input_terminal(&peer, peer_first.subscription, 0, OperationId::new(), b"x"),
            Err(TerminalError::Unavailable)
        );

        // Re-attaching on the fresh epoch restores both panes on the replacement
        // connection, and each one's input sequence starts from that connection's
        // own ledger.
        let (replacement, second) = scripted_terminal_connection(
            vec![
                (ResponseOutcome::Ok, attach_body(21)),
                (ResponseOutcome::Ok, attach_body(22)),
                (ResponseOutcome::Ok, json!({ "ack": "Written" })),
                (ResponseOutcome::Ok, json!({ "ack": "Written" })),
            ],
            |_| {},
        );
        port.terminal = Some(replacement);
        let agent_second = port.attach_terminal(&agent, geometry).unwrap();
        let peer_second = port.attach_terminal(&peer, geometry).unwrap();
        assert_eq!(
            agent_second.subscription,
            TerminalSubscription { id: 21, epoch: 2 }
        );
        assert_eq!(
            peer_second.subscription,
            TerminalSubscription { id: 22, epoch: 2 }
        );
        for (terminal, attached) in [(&agent, agent_second), (&peer, peer_second)] {
            assert_eq!(
                port.input_terminal(terminal, attached.subscription, 0, OperationId::new(), b"k"),
                Ok(TerminalInputOutcome::Written)
            );
        }
        drop(port);
        assert_eq!(
            terminal_actions(second.join().unwrap()),
            vec!["attach", "attach", "input#0@21", "input#0@22"]
        );
    }

    /// A resize failure drops only its own lane: the shared attach / input
    /// connection, its epoch, and every pane's subscription survive it.
    #[cfg(unix)]
    #[test]
    fn production_resize_lane_failure_keeps_the_shared_connection_and_epoch() {
        use usagi_core::infrastructure::ipc::ResponseOutcome;
        use usagi_tui::presentation::AgentCommandPort;

        let geometry = Geometry { cols: 20, rows: 3 };
        let (mut port, server) =
            terminal_input_port(vec![(ResponseOutcome::Ok, json!({ "ack": "Written" }))]);
        // The resize lane answers nothing and closes, which is the read timeout /
        // EOF the deadline-bounded lane is there to contain.
        let (resize_lane, resize_server) = scripted_terminal_connection(Vec::new(), |_| {});
        port.poll = Some(resize_lane);
        let terminal = input_terminal_ref();

        assert_eq!(
            port.resize_terminal(&terminal, geometry),
            Err(TerminalError::Unavailable)
        );

        assert!(port.poll.is_none());
        assert!(port.terminal.is_some());
        assert_eq!(port.terminal_connection_epoch(), Some(1));
        // The subscription taken before the resize is still current, so the pane
        // keeps writing without reattaching.
        assert_eq!(
            port.input_terminal(&terminal, subscription(7), 0, OperationId::new(), b"x"),
            Ok(TerminalInputOutcome::Written)
        );

        drop(port);
        server.join().unwrap();
        resize_server.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn production_malformed_attach_on_same_socket_keeps_epoch_and_next_input_sequence() {
        use usagi_core::infrastructure::ipc::ResponseOutcome;
        use usagi_core::usecase::client::{DaemonRequest, TerminalAction, TerminalRequest};
        use usagi_tui::presentation::AgentCommandPort;

        let valid_attach = json!({
            "subscription": 8,
            "snapshot": {
                "revision": 3,
                "output_offset": 0,
                "base_offset": 0,
                "screen": screen_checkpoint_value(b"", 3, 20),
                "exited": null
            }
        });
        let (mut port, server) = terminal_input_port(vec![
            (ResponseOutcome::Ok, json!({ "ack": "Written" })),
            (ResponseOutcome::Ok, json!({ "snapshot": {} })),
            (ResponseOutcome::Ok, valid_attach),
            (ResponseOutcome::Ok, json!({ "ack": "Written" })),
        ]);
        let terminal = input_terminal_ref();

        assert_eq!(
            port.input_terminal(&terminal, subscription(7), 0, OperationId::new(), b"a"),
            Ok(TerminalInputOutcome::Written)
        );
        assert_eq!(
            port.attach_terminal(&terminal, Geometry { cols: 20, rows: 3 }),
            Err(TerminalError::Unavailable)
        );
        assert!(port.terminal.is_some());
        let attach = port
            .attach_terminal(&terminal, Geometry { cols: 20, rows: 3 })
            .unwrap();
        assert_eq!(attach.subscription.epoch, 1);
        assert_eq!(
            port.input_terminal(&terminal, attach.subscription, 1, OperationId::new(), b"b"),
            Ok(TerminalInputOutcome::Written)
        );

        drop(port);
        let requests = server.join().unwrap();
        let input_sequences = requests
            .into_iter()
            .filter_map(|body| serde_json::from_value::<DaemonRequest>(body).ok())
            .filter_map(|request| match request {
                DaemonRequest::Terminal {
                    action: TerminalAction::Input,
                    payload,
                } => serde_json::from_value::<TerminalRequest>(payload).ok(),
                _ => None,
            })
            .filter_map(|request| match request {
                TerminalRequest::Input { input_seq, .. } => Some(input_seq),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(input_sequences, vec![0, 1]);
    }

    #[cfg(unix)]
    #[test]
    fn production_attach_uses_checkpoints_and_never_parses_a_legacy_tail() {
        use usagi_core::infrastructure::ipc::{
            ProtocolRange, ResponseOutcome, TERMINAL_SCREEN_CHECKPOINT_CAPABILITY,
            TERMINAL_WIRE_GENERATION,
        };
        use usagi_tui::presentation::AgentCommandPort;

        type Adjust = fn(&mut usagi_core::infrastructure::ipc::ServerProtocol);

        // A raw tail cut mid-CSI. Whatever the negotiation outcome, no path may
        // decode these bytes into a screen.
        let truncated_tail = b"\x1b[31mred\x1b[".to_vec();
        let snapshot = |replay: &[u8]| {
            json!({
                "subscription": 4,
                "snapshot": {
                    "revision": 2,
                    "base_offset": 0,
                    "output_offset": 0,
                    "screen": screen_checkpoint_value(b"hi", 3, 20),
                    "replay": replay,
                    "exited": null
                }
            })
        };

        let cases: [(&str, Adjust, bool); 3] = [
            // New client × new daemon: capability present, common revision 2.
            ("checkpoint", |_| {}, true),
            // New client × new-enough revision but no capability advertised:
            // the capability is the truth source, so this stays legacy.
            (
                "capability absent",
                |protocol| {
                    protocol
                        .capabilities
                        .retain(|capability| capability != TERMINAL_SCREEN_CHECKPOINT_CAPABILITY);
                },
                false,
            ),
            // New client × old daemon: the common revision falls back to 1.
            (
                "revision 1 daemon",
                |protocol| {
                    protocol.supported_protocols = vec![ProtocolRange {
                        generation: TERMINAL_WIRE_GENERATION,
                        min_revision: 0,
                        max_revision: 1,
                    }];
                },
                false,
            ),
        ];

        for (name, adjust, expects_checkpoint) in cases {
            let (mut port, server) = terminal_input_port_with(
                vec![(ResponseOutcome::Ok, snapshot(&truncated_tail))],
                adjust,
            );
            let attach = port
                .attach_terminal(&input_terminal_ref(), Geometry { cols: 20, rows: 3 })
                .expect("scripted attach decodes");

            match attach.screen {
                TerminalAttachScreen::Checkpoint(checkpoint) => {
                    assert!(
                        expects_checkpoint,
                        "{name} must not use the checkpoint path"
                    );
                    assert_eq!(checkpoint.geometry.rows, 3);
                    assert_eq!(checkpoint.geometry.cols, 20);
                }
                TerminalAttachScreen::HistoryUnavailable => {
                    assert!(!expects_checkpoint, "{name} must use the checkpoint path");
                }
            }
            assert_eq!(attach.revision, 2, "{name}");
            drop(port);
            server.join().unwrap();
        }
    }

    #[test]
    fn attach_screen_decoder_fails_closed_on_an_unusable_checkpoint_frame() {
        let checkpoint = screen_checkpoint_value(b"ok", 2, 4);
        let frame = |base: u64, screen: serde_json::Value| json!({ "revision": 1, "base_offset": base, "output_offset": 9, "screen": screen });

        // A checkpoint is complete at `output_offset`; a frame that also claims a
        // tail is refused instead of being restored and then double-fed.
        assert_eq!(
            decode_attach_screen(
                TerminalSnapshotMode::Checkpoint,
                &frame(0, checkpoint.clone()),
                0,
                9,
            ),
            Err(TerminalError::Unavailable)
        );
        // A missing or malformed checkpoint is refused rather than approximated.
        for screen in [json!(null), json!({ "schema_version": 1 })] {
            assert_eq!(
                decode_attach_screen(TerminalSnapshotMode::Checkpoint, &frame(9, screen), 9, 9),
                Err(TerminalError::Unavailable)
            );
        }
        assert!(matches!(
            decode_attach_screen(
                TerminalSnapshotMode::Checkpoint,
                &frame(9, checkpoint),
                9,
                9
            ),
            Ok(TerminalAttachScreen::Checkpoint(_))
        ));
        // The legacy path ignores the frame entirely: an offset mismatch or a
        // mid-escape tail can never turn into parsed screen state.
        assert_eq!(
            decode_attach_screen(
                TerminalSnapshotMode::LegacyFailClosed,
                &json!({ "replay": b"\x1b[31m".to_vec() }),
                0,
                9,
            ),
            Ok(TerminalAttachScreen::HistoryUnavailable)
        );
    }

    #[test]
    fn terminal_input_ack_decoder_rejects_malformed_or_unsafe_outcomes() {
        let invalid = [
            json!(null),
            json!({}),
            json!({ "ack": "Unknown" }),
            json!({ "ack": "Written", "extra": true }),
            json!({ "ack": { "Ambiguous": { "applied_prefix": 0 } } }),
            json!({ "ack": { "Ambiguous": { "applied_prefix": 4 } } }),
            json!({ "ack": { "Ambiguous": { "applied_prefix": 1, "extra": true } } }),
            json!({ "ack": { "Cached": { "Other": "Written" } } }),
        ];
        for body in invalid {
            assert_eq!(
                decode_terminal_input_ack(&body, 3),
                Err(TerminalError::InputEffectUnknown)
            );
        }

        let mut too_deep = json!("Written");
        for _ in 0..=super::MAX_CACHED_INPUT_ACK_DEPTH {
            too_deep = json!({ "Cached": too_deep });
        }
        assert_eq!(
            decode_terminal_input_ack(&json!({ "ack": too_deep }), 1),
            Err(TerminalError::InputEffectUnknown)
        );
    }

    #[cfg(unix)]
    #[test]
    fn production_terminal_input_reports_ack_loss_as_unknown_without_resend() {
        use std::os::unix::net::UnixStream;

        use usagi_core::infrastructure::ipc::{
            BuildIdentity, DaemonGeneration, Envelope, read_json_frame,
        };
        use usagi_core::usecase::client::{ClientPolicy, IpcClient};
        use usagi_daemon::presentation::ipc::{handshake, server_protocol};
        use usagi_tui::presentation::AgentCommandPort;

        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let build = BuildIdentity {
            version: "test".to_owned(),
            commit: "test".to_owned(),
            target: "test".to_owned(),
            artifact: "test-artifact".to_owned(),
        };
        let protocol = server_protocol(
            DaemonGeneration("input-ack-loss-test".to_owned()),
            "input-ack-loss-connection".to_owned(),
            build.clone(),
            usagi_core::domain::daemon::DaemonRecord::identified(2, "test-process"),
            TEST_WORKSPACE_ROOT.to_owned(),
        );
        let server = std::thread::spawn(move || {
            let mut reader = server_stream.try_clone().unwrap();
            let mut writer = server_stream;
            let hello = handshake(&mut reader, &mut writer, &protocol)
                .unwrap()
                .unwrap();
            let request =
                read_json_frame::<Envelope>(&mut reader, hello.limits.max_frame_bytes as usize)
                    .unwrap();
            assert!(
                request.is_some(),
                "exactly one input request reaches the server"
            );
            // Close after consuming the request but before writing its ACK.
        });
        let client = IpcClient::connect(
            crate::runtime::daemon::deadline_transport(
                crate::runtime::daemon::SystemClock::new(),
                client_stream,
                ClientPolicy::tui().timeout_ms,
            ),
            "input-ack-loss-client".to_owned(),
            "input-ack-loss-nonce".to_owned(),
            ClientPolicy::tui(),
            build,
            test_client_workspace(),
        )
        .unwrap();
        let mut port = DaemonAgentCommandPort {
            terminal: Some(client),
            poll: None,
            pump: TerminalPollPump::spawn(|_| Ok(Vec::new())),
            inventory: None,
            terminal_epoch: 1,
            attachments: Vec::new(),
            restore_connection: None,
            terminal_watch_cancelled: None,
        };

        assert_eq!(
            port.input_terminal(
                &input_terminal_ref(),
                subscription(7),
                0,
                OperationId::new(),
                b"x"
            ),
            Err(TerminalError::InputEffectUnknown)
        );
        assert!(port.terminal.is_none());
        server.join().unwrap();
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
        let body = json!({ "output": [{"start_offset": 0, "data": b"abc".to_vec()}] });
        assert_eq!(decode_terminal_poll(&body), Err(TerminalError::Unavailable));
    }

    #[test]
    fn terminal_inventory_decode_is_all_or_nothing() {
        let terminal = TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: WorkspaceId::new(),
            session_id: None,
            worktree_id: WorktreeId::new(),
        };
        let entry = usagi_core::domain::terminal_launch::TerminalInventoryEntry {
            terminal: terminal.clone(),
            kind: usagi_core::domain::terminal_launch::TerminalKind::Terminal,
            live: true,
        };
        assert_eq!(
            decode_terminal_inventory(&json!({"terminals": [entry.clone()]})),
            Ok(vec![entry.clone()])
        );
        assert_eq!(
            decode_terminal_inventory(&json!({"terminals": [{"live": true}]})),
            Err(TerminalError::Unavailable)
        );
        assert_eq!(
            decode_terminal_inventory(&json!({})),
            Err(TerminalError::Unavailable)
        );
        let scope = TerminalLaunchScope {
            workspace_id: terminal.workspace_id,
            session_id: terminal.session_id,
            worktree_id: terminal.worktree_id,
        };
        assert!(terminal_inventory_matches_scope(
            std::slice::from_ref(&entry),
            &scope
        ));
        let mut wrong_worktree = entry;
        wrong_worktree.terminal.worktree_id = WorktreeId::new();
        assert!(!terminal_inventory_matches_scope(&[wrong_worktree], &scope));
    }

    #[test]
    fn passive_restore_socket_eof_emits_one_reconnect_epoch_and_drop_cancels_watchers() {
        use usagi_daemon::infrastructure::unix_transport::{SecureUnixListener, connect_current};

        let temporary = tempfile::tempdir().unwrap();
        let listener = SecureUnixListener::bind(
            temporary.path(),
            usagi_core::infrastructure::ipc::DaemonGeneration("restore-watch".to_owned()),
        )
        .unwrap();
        let stream = connect_current(temporary.path()).unwrap();
        let peer = loop {
            match listener.accept() {
                Ok(peer) => break peer,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::yield_now();
                }
                Err(error) => panic!("fixture listener failed: {error}"),
            }
        };
        let (mut events, publisher) =
            DaemonRestoreConnectionPort::channel(temporary.path().to_path_buf());
        let _watch = publisher.watch(stream);
        std::thread::sleep(Duration::from_millis(75));
        assert_eq!(events.take_reconnected_epoch(), None);

        drop(peer);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let epoch = loop {
            if let Some(epoch) = events.take_reconnected_epoch() {
                break epoch;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "passive EOF watcher did not publish reconnect"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(epoch, 1);
        assert_eq!(events.take_reconnected_epoch(), None);

        let cancelled = Arc::clone(&publisher.cancelled);
        drop(events);
        assert!(cancelled.load(Ordering::Acquire));
    }

    #[test]
    fn agent_admission_decodes_only_fenced_terminal_and_opaque_continuation() {
        let workspace = WorkspaceId::new();
        let terminal = TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: workspace,
            session_id: None,
            worktree_id: WorktreeId::new(),
        };
        let continuation = AgentContinuationRef::new();
        let admission = decode_agent_admission(
            &json!({"terminal": terminal, "continuation": continuation}),
            "agent launch",
        )
        .unwrap();
        assert_eq!(admission.terminal, terminal);
        assert_eq!(admission.continuation, Some(continuation));
        assert_eq!(
            decode_agent_admission(&json!({"terminal": terminal}), "legacy launch")
                .unwrap()
                .continuation,
            None
        );
        assert_eq!(
            decode_agent_admission(&json!({}), "agent launch").unwrap_err(),
            "agent launch returned no terminal"
        );
        assert_eq!(
            decode_agent_admission(
                &json!({"terminal": "bad", "continuation": continuation}),
                "agent launch",
            )
            .unwrap_err(),
            "agent launch returned an invalid terminal"
        );
        assert_eq!(
            decode_agent_admission(
                &json!({"terminal": terminal, "continuation": "provider-native-id"}),
                "agent launch",
            )
            .unwrap_err(),
            "agent launch returned an invalid continuation"
        );
    }

    /// #510: the exact-target resume answer carries the daemon's own lineage and
    /// source-to-replacement relation, and never infers either.
    #[test]
    fn exact_agent_resume_decodes_the_daemon_relation_or_leaves_it_absent() {
        use usagi_core::domain::agent::AgentResumeRelation;
        use usagi_core::domain::id::{AgentResumeSourceId, AgentRuntimeId};

        let workspace = WorkspaceId::new();
        let terminal = TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: workspace,
            session_id: None,
            worktree_id: WorktreeId::new(),
        };
        let continuation = AgentContinuationRef::new();
        let relation = AgentResumeRelation {
            source: AgentResumeSourceId::new(),
            replacement_runtime: AgentRuntimeId::new(),
            replacement_terminal: terminal.clone(),
        };

        let resume = decode_exact_agent_resume(&json!({
            "terminal": terminal,
            "continuation": continuation,
            "resume_relation": relation,
        }))
        .unwrap();
        assert_eq!(resume.terminal, terminal);
        assert_eq!(resume.continuation, Some(continuation));
        assert_eq!(resume.relation, Some(relation));

        // A body without a decodable relation or lineage yields none of either,
        // so the TUI refuses the replacement instead of assuming one.
        let bare = decode_exact_agent_resume(&json!({
            "terminal": terminal,
            "continuation": null,
            "resume_relation": "provider-native-id",
        }))
        .unwrap();
        assert_eq!(bare.continuation, None);
        assert_eq!(bare.relation, None);

        assert_eq!(
            decode_exact_agent_resume(&json!({})).unwrap_err(),
            "provider resume returned no terminal"
        );
        assert_eq!(
            decode_exact_agent_resume(&json!({"terminal": "bad"})).unwrap_err(),
            "provider resume returned an invalid terminal"
        );
    }

    #[test]
    fn lifecycle_snapshot_lists_failed_and_deleting_sessions_with_their_lifecycle_projection() {
        use usagi_core::domain::session_lifecycle::{Failure, FailureStage};
        let workspace = Workspace::new("work", "/tmp/work");
        let mut available =
            ManagedSession::new_creating("available".into(), OperationId::new(), Utc::now());
        available.lifecycle = SessionLifecycle::Available;
        let mut failed =
            ManagedSession::new_creating("failed".into(), OperationId::new(), Utc::now());
        failed.lifecycle = SessionLifecycle::Failed;
        failed.failure = Some(Failure {
            stage: FailureStage::Create,
            summary: "create failed".into(),
        });
        // An accepted removal whose daemon-owned teardown is still running.
        let mut deleting =
            ManagedSession::new_creating("deleting".into(), OperationId::new(), Utc::now());
        deleting.lifecycle = SessionLifecycle::Deleting;
        // A transient reservation is durable but not a sidebar row.
        let creating =
            ManagedSession::new_creating("creating".into(), OperationId::new(), Utc::now());
        let available_id = available.session_id;
        let failed_id = failed.session_id;
        let deleting_id = deleting.session_id;
        let snapshot = LifecycleSnapshot {
            workspace_id: WorkspaceId::new(),
            root_worktree_id: usagi_core::domain::id::WorktreeId::new(),
            revision: 1,
            sessions: vec![available, failed, deleting, creating],
            agent_resumes: std::collections::BTreeMap::new(),
        };

        // Available, Failed and Deleting are listed; the Creating row is not.
        assert_eq!(
            snapshot
                .listed_sessions()
                .map(|session| session.name.as_str())
                .collect::<Vec<_>>(),
            ["available", "failed", "deleting"]
        );
        // Every listed row is projected, so a Failed row's name is visible and a
        // removal in progress keeps its row until the teardown finishes.
        assert_eq!(
            snapshot
                .project(&workspace, &[])
                .iter()
                .map(|record| record.name.clone())
                .collect::<Vec<_>>(),
            ["available", "failed", "deleting"]
        );
        // The lifecycle projection carries each state and the Failed summary.
        let lifecycles = snapshot.session_lifecycles();
        assert_eq!(
            lifecycles.get(&available_id).unwrap().lifecycle,
            SessionLifecycle::Available
        );
        let failed_projection = lifecycles.get(&failed_id).unwrap();
        assert_eq!(failed_projection.lifecycle, SessionLifecycle::Failed);
        assert!(!failed_projection.capabilities().can_use);
        assert!(failed_projection.capabilities().can_remove);
        assert_eq!(
            failed_projection.failure_summary.as_deref(),
            Some("create failed")
        );
        // A Deleting row is neither attachable nor removable again, so listing
        // it cannot produce a second teardown of the same session.
        let deleting_projection = lifecycles.get(&deleting_id).unwrap();
        assert_eq!(deleting_projection.lifecycle, SessionLifecycle::Deleting);
        assert!(!deleting_projection.capabilities().can_use);
        assert!(!deleting_projection.capabilities().can_remove);
        assert_eq!(deleting_projection.failure_summary, None);
    }

    #[test]
    fn provider_resume_projection_accepts_only_the_safe_typed_wire_vocabulary() {
        let session = SessionId::new();
        let item = json!({
            "session_id": session,
            "agent_phase": "interrupted",
            "agent_resumable": true,
            "agent_resume_reason": "explicit_resume_available",
        });
        assert_eq!(
            provider_resume_projection(&item).unwrap(),
            Some((
                session,
                ProviderResumeProjection {
                    interrupted: true,
                    resumable: true,
                    reason: ProviderResumeReason::ExplicitResumeAvailable,
                },
            ))
        );
        assert_eq!(provider_resume_projection(&json!({})).unwrap(), None);
        assert_eq!(
            provider_resume_projection(&json!({
                "session_id": session,
                "agent_phase": "running",
                "agent_resumable": false,
                "agent_resume_reason": "live_or_ownership_unknown",
            }))
            .unwrap(),
            Some((
                session,
                ProviderResumeProjection {
                    interrupted: false,
                    resumable: false,
                    reason: ProviderResumeReason::LiveOrOwnershipUnknown,
                },
            ))
        );
        let malformed = [
            json!({ "agent_phase": 1 }),
            json!({ "agent_phase": "interrupted" }),
            json!({
                "session_id": "not-a-session-id",
                "agent_phase": "interrupted",
                "agent_resumable": false,
                "agent_resume_reason": "provider_metadata_unavailable",
            }),
            json!({
                "session_id": session,
                "agent_phase": "interrupted",
                "agent_resume_reason": "provider_metadata_unavailable",
            }),
            json!({
                "session_id": session,
                "agent_phase": "interrupted",
                "agent_resumable": false,
            }),
        ];
        for item in malformed {
            assert!(provider_resume_projection(&item).is_err());
        }
        assert!(
            provider_resume_projection(&json!({
                "session_id": session,
                "agent_phase": "interrupted",
                "agent_resumable": false,
                "agent_resume_reason": "provider raw output",
            }))
            .is_err()
        );
    }

    #[test]
    fn tui_agent_resume_helpers_use_the_shared_exact_wire_contract() {
        let workspace = WorkspaceId::new();
        let operation = OperationId::new();
        let target = usagi_core::domain::agent::AgentResumeTarget {
            continuation: usagi_core::domain::id::AgentContinuationRef::new(),
            source: usagi_core::domain::id::AgentResumeSourceId::new(),
            workspace_id: workspace,
            session_id: None,
            worktree_id: usagi_core::domain::id::WorktreeId::new(),
            runtime_id: usagi_core::domain::id::AgentRuntimeId::new(),
            adapter_revision: 1,
        };
        assert_eq!(
            agent_inventory_request(workspace),
            usagi_core::usecase::client::DaemonRequest::AgentInventory { workspace }
        );
        assert_eq!(
            exact_agent_resume_request(operation, target.clone()),
            usagi_core::usecase::client::DaemonRequest::ResumeAgent {
                operation_id: operation.to_string(),
                target,
            }
        );
    }

    #[test]
    fn lifecycle_parser_projection_and_safe_error_mapping_cover_every_branch() {
        use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};
        use usagi_core::usecase::client::ClientError;

        let safe = DaemonDecisionCommandPort::safe_error("decision failed");
        assert_eq!(safe.message.as_str(), "decision failed");
        assert_eq!(safe.error_id, "decision-daemon-error");

        for value in [
            json!(null),
            json!({}),
            json!({"revision": 1}),
            json!({"revision": 1, "workspace_id": "bad"}),
            json!({"revision": 1, "workspace_id": WorkspaceId::new()}),
            json!({
                "revision": 1,
                "workspace_id": WorkspaceId::new(),
                "root_worktree_id": "bad"
            }),
            json!({
                "revision": 1,
                "workspace_id": WorkspaceId::new(),
                "root_worktree_id": usagi_core::domain::id::WorktreeId::new()
            }),
            json!({
                "revision": 1,
                "workspace_id": WorkspaceId::new(),
                "root_worktree_id": usagi_core::domain::id::WorktreeId::new(),
                "sessions": "bad"
            }),
        ] {
            assert!(lifecycle_snapshot(&value).is_err());
        }

        let mut managed =
            ManagedSession::new_creating("fresh".into(), OperationId::new(), Utc::now());
        managed.lifecycle = SessionLifecycle::Available;
        let workspace_id = WorkspaceId::new();
        let worktree_id = usagi_core::domain::id::WorktreeId::new();
        let parsed = lifecycle_snapshot(&json!({
            "revision": 7,
            "workspace_id": workspace_id,
            "root_worktree_id": worktree_id,
            "sessions": [managed]
        }))
        .unwrap();
        assert_eq!(parsed.revision, 7);

        let temporary = tempfile::tempdir().unwrap();
        let workspace = Workspace::new("repo", temporary.path());
        let result = session_snapshot_result("ok", &parsed, &workspace).unwrap();
        assert_eq!(result.message, "ok");
        assert_eq!(result.sessions.unwrap()[0].name, "fresh");
        assert_eq!(result.session_ids.unwrap().len(), 1);

        for (code, expected) in [
            (ErrorCode::ResyncRequired, TerminalError::ResyncRequired),
            (ErrorCode::StaleTarget, TerminalError::Stale),
            (ErrorCode::OwnershipUnknown, TerminalError::Orphaned),
            (ErrorCode::Internal, TerminalError::Unavailable),
        ] {
            let error = ClientError::Protocol(ProtocolError::new(code, "safe"));
            assert_eq!(map_terminal_error(&error), expected);
            assert_eq!(daemon_error_reason(error), "safe");
        }
        assert_eq!(
            daemon_error_reason(ClientError::Unavailable("offline".into())),
            "offline"
        );
        assert_eq!(
            daemon_error_reason(ClientError::Lifecycle("restart".into())),
            "restart"
        );
        // Contention is a "someone else is connecting" notice, not an outage:
        // the surface says it is retrying rather than that the daemon is gone.
        assert_eq!(
            daemon_error_reason(ClientError::BootstrapContended),
            "another usagi process is establishing the daemon connection; retrying"
        );

        let file = temporary.path().join("file");
        std::fs::write(&file, "x").unwrap();
        assert!(matches!(
            probe_path(temporary.path()),
            usagi_core::usecase::workspace::WorkspaceProbe::Directory
        ));
        assert!(matches!(
            probe_path(&file),
            usagi_core::usecase::workspace::WorkspaceProbe::NonDirectory
        ));
        assert!(matches!(
            probe_path(&temporary.path().join("missing")),
            usagi_core::usecase::workspace::WorkspaceProbe::Missing
        ));
    }

    #[test]
    fn build_identity_errors_keep_the_old_daemon_in_the_tui_message() {
        use usagi_core::usecase::client::ClientError;

        let running = usagi_core::infrastructure::ipc::build_identity(
            "1",
            "a",
            "test",
            "debug",
            &"a".repeat(64),
        );
        let expected = usagi_core::infrastructure::ipc::build_identity(
            "1",
            "b",
            "test",
            "debug",
            &"b".repeat(64),
        );
        let trigger = usagi_core::infrastructure::ipc::build_rollover_trigger(
            &running, &expected, "local", false,
        )
        .unwrap();
        assert!(
            daemon_error_reason(ClientError::RolloverRequired(trigger))
                .contains("current daemon remains running")
        );
        assert_eq!(
            daemon_error_reason(ClientError::BuildIdentityUnavailable),
            "exact daemon build identity is unavailable; the current daemon remains running"
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
            agent_resumes: std::collections::BTreeMap::new(),
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
        for (code, expected) in [
            (KeyCode::Up, Key::Up),
            (KeyCode::Down, Key::Down),
            (KeyCode::Left, Key::Left),
            (KeyCode::Right, Key::Right),
            (KeyCode::Home, Key::Home),
            (KeyCode::End, Key::End),
            (KeyCode::Delete, Key::Delete),
            (KeyCode::Tab, Key::Tab),
            (KeyCode::Backspace, Key::Backspace),
            (KeyCode::Escape, Key::Escape),
            (KeyCode::Unknown, Key::Other),
        ] {
            assert_eq!(
                passthrough_key(&live_key(code, Modifiers::default()), Vec::new()),
                expected
            );
        }
        assert_eq!(
            passthrough_key(
                &LiveInput::Pointer(usagi_tui::usecase::terminal_input::PointerEvent {
                    kind: usagi_tui::usecase::terminal_input::PointerKind::Up,
                    column: 0,
                    row: 0,
                }),
                Vec::new(),
            ),
            Key::Other
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
    fn terminal_adapter_maps_global_chords_after_classifier_resolution() {
        let cases = [
            (live_key(KeyCode::Char('c'), control()), Key::Quit),
            (LiveInput::Raw(vec![3]), Key::Quit),
            (live_key(KeyCode::Char('q'), control()), Key::CtrlQ),
            (LiveInput::Raw(vec![17]), Key::CtrlQ),
            (live_key(KeyCode::Char('d'), control()), Key::CtrlD),
            (LiveInput::Raw(vec![4]), Key::CtrlD),
        ];
        for (input, expected) in cases {
            assert_eq!(
                classify_terminal_input(
                    &mut usagi_tui::usecase::terminal_input::LiveInputClassifier::default(),
                    Duration::ZERO,
                    &input,
                ),
                Some(expected)
            );
        }
    }

    #[test]
    fn terminal_adapter_swallows_leader_global_follow_up_then_reads_next_key_fresh() {
        for follow_up in [
            live_key(KeyCode::Char('c'), control()),
            LiveInput::Raw(vec![3]),
            live_key(KeyCode::Char('q'), control()),
            LiveInput::Raw(vec![17]),
            live_key(KeyCode::Char('d'), control()),
            LiveInput::Raw(vec![4]),
        ] {
            let mut classifier = usagi_tui::usecase::terminal_input::LiveInputClassifier::default();
            assert_eq!(
                classify_terminal_input(
                    &mut classifier,
                    Duration::ZERO,
                    &live_key(KeyCode::Char('o'), control()),
                ),
                None
            );
            assert_eq!(
                classify_terminal_input(&mut classifier, Duration::from_millis(1), &follow_up,),
                None
            );
            assert_eq!(
                classify_terminal_input(
                    &mut classifier,
                    Duration::from_millis(2),
                    &live_key(KeyCode::Char('z'), Modifiers::default()),
                ),
                Some(Key::Char('z'))
            );
        }
    }

    #[test]
    fn terminal_adapter_projects_resolved_live_and_pointer_inputs() {
        let cases = [
            (
                LiveInput::WheelUp,
                Key::Live(usagi_tui::usecase::terminal_input::LiveTerminalAction::ScrollUp),
            ),
            (
                LiveInput::Pointer(PointerEvent {
                    kind: PointerKind::Drag,
                    column: 7,
                    row: 11,
                }),
                Key::Pointer(PointerEvent {
                    kind: PointerKind::Drag,
                    column: 7,
                    row: 11,
                }),
            ),
            (
                LiveInput::Mouse { column: 5, row: 9 },
                Key::Click { column: 5, row: 9 },
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(
                classify_terminal_input(
                    &mut usagi_tui::usecase::terminal_input::LiveInputClassifier::default(),
                    Duration::ZERO,
                    &input,
                ),
                Some(expected)
            );
        }
    }

    #[test]
    fn native_terminal_copy_shortcut_is_selection_aware() {
        let modifiers = {
            #[cfg(target_os = "macos")]
            {
                Modifiers {
                    super_: true,
                    ..Modifiers::default()
                }
            }
            #[cfg(target_os = "windows")]
            {
                control()
            }
            #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
            {
                Modifiers {
                    control: true,
                    shift: true,
                    ..Modifiers::default()
                }
            }
        };
        let fallback = {
            #[cfg(target_os = "windows")]
            {
                vec![3]
            }
            #[cfg(not(target_os = "windows"))]
            {
                Vec::new()
            }
        };

        assert_eq!(
            terminal_copy_key(&live_key(KeyCode::Char('c'), modifiers)),
            Some(Key::TerminalCopy { fallback })
        );
        assert_eq!(terminal_copy_key(&LiveInput::Text("c".into())), None);
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

        let shifted_enter = LiveInput::Key(KeyEvent::new(
            KeyCode::Enter,
            Modifiers {
                shift: true,
                ..Modifiers::default()
            },
            KeyEventKind::Press,
        ));
        assert_eq!(
            passthrough_key(&shifted_enter, b"\r".to_vec()),
            Key::Passthrough(b"\r".to_vec())
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
            LiveInput::Raw(b"\n".to_vec()),
            LiveInput::Text("\n".to_owned()),
        ] {
            assert_eq!(passthrough_key(&input, Vec::new()), Key::Enter);
        }
    }

    #[test]
    fn a_paste_becomes_a_paste_key_while_raw_and_text_stay_passthrough() {
        // A bracketed paste carries its decoded text so the focused pane can wrap
        // it in bracketed-paste markers before forwarding it to the PTY.
        let pasted = "line1\nline2".as_bytes().to_vec();
        assert_eq!(
            passthrough_key(&LiveInput::Paste(pasted.clone()), pasted),
            Key::Paste("line1\nline2".to_owned())
        );
        // Raw and text payloads keep their exact bytes as opaque passthrough.
        let raw = b"\x1b[99~".to_vec();
        assert_eq!(
            passthrough_key(&LiveInput::Raw(raw.clone()), raw.clone()),
            Key::Passthrough(raw)
        );
        let text = "xy".as_bytes().to_vec();
        assert_eq!(
            passthrough_key(&LiveInput::Text("xy".to_owned()), text.clone()),
            Key::Passthrough(text)
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
    fn welcome_start_loads_or_projects_storage_errors() {
        let healthy = tempfile::tempdir().unwrap();
        let (workspaces, recent) =
            load_screen_graph_data(&Storage::new(healthy.path()), Start::Welcome).unwrap();
        assert!(workspaces.is_empty());
        assert!(recent.is_empty());

        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("workspaces.json"), "{ broken").unwrap();
        let storage = Storage::new(home.path());
        let (workspaces, recent) = load_screen_graph_data(&storage, Start::Config).unwrap();
        assert!(workspaces.is_empty());
        assert!(recent.is_empty());
        assert!(load_screen_graph_data(&storage, Start::Welcome).is_err());
    }

    #[test]
    fn workspace_directory_validation_projects_metadata_errors() {
        let temporary = tempfile::tempdir().unwrap();
        let missing = temporary.path().join("missing");
        assert_eq!(
            validate_workspace_directory(&missing).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[test]
    fn opening_a_workspace_this_daemon_does_not_serve_is_a_presentable_refusal() {
        let opened = std::path::Path::new("/workspace/other");
        let refusal = usagi_core::infrastructure::ipc::workspace_admission(
            Some(
                &usagi_core::infrastructure::ipc::ClientWorkspace::Selected {
                    root: opened.display().to_string(),
                },
            ),
            "/workspace/root",
        )
        .unwrap_err();

        // The handshake refusal arrives while connecting, and it becomes the one
        // error kind the entry screens present in place.
        let error = workspace_open_error(
            LifecycleRequestError::Connect(ClientError::Protocol(refusal.clone())),
            opened,
        );
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        let message = error.to_string();
        // The daemon's own wording (which names the workspace that *is* served),
        // the workspace that was asked for, and the one recovery step.
        assert!(message.contains(&refusal.message), "{message}");
        assert!(message.contains("/workspace/root"), "{message}");
        assert!(message.contains("/workspace/other"), "{message}");
        assert!(message.contains("usagi daemon stop"), "{message}");

        // A refusal that arrives on the request instead is the same refusal.
        assert_eq!(
            workspace_open_error(
                LifecycleRequestError::Request(ClientError::Protocol(refusal)),
                opened,
            )
            .kind(),
            std::io::ErrorKind::PermissionDenied
        );

        // Everything else keeps the single-line reason every other surface shows,
        // and must not be mistaken for a workspace the user can switch to.
        for (error, expected) in [
            (
                LifecycleRequestError::Connect(ClientError::Unavailable("offline".into())),
                "daemon unavailable: Unavailable: offline",
            ),
            (
                LifecycleRequestError::Request(ClientError::Protocol(
                    usagi_core::infrastructure::ipc::ProtocolError::new(
                        usagi_core::infrastructure::ipc::ErrorCode::StaleTarget,
                        "workspace is no longer available",
                    ),
                )),
                "workspace is no longer available",
            ),
            (
                LifecycleRequestError::Decode("daemon returned an invalid snapshot".to_owned()),
                "daemon returned an invalid snapshot",
            ),
        ] {
            let error = workspace_open_error(error, opened);
            assert_ne!(error.kind(), std::io::ErrorKind::PermissionDenied);
            assert_eq!(error.to_string(), expected);
        }
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
    fn repo_store_resolves_targets_and_reports_a_stale_session() {
        let workspace = tempfile::tempdir().unwrap();
        let alpha = SessionId::new();
        let store = RepoEnvironmentStore::new(
            workspace.path(),
            vec![(alpha, "alpha".to_owned())],
            SettingsEnvironmentStore::new(workspace.path().to_path_buf(), workspace.path()),
        );

        // The root always resolves; a known session resolves to its store name.
        assert!(matches!(
            store.resolve(Target::Root(WorkspaceId::new())),
            Some(StoreTarget::Root)
        ));
        assert!(matches!(
            store.resolve(Target::Session(alpha)),
            Some(StoreTarget::Session("alpha"))
        ));
        // A session absent from the snapshot mapping is stale, not guessed.
        assert!(store.resolve(Target::Session(SessionId::new())).is_none());

        let stale = RepoEnvironmentStore::stale_target();
        assert_eq!(stale.error_id, "target-store-error");
        assert!(stale.message.as_str().contains("no longer available"));
        assert!(
            RepoEnvironmentStore::safe_error(anyhow::anyhow!("state.json is unreadable"))
                .message
                .as_str()
                .contains("state.json is unreadable")
        );
    }

    #[test]
    fn settings_environment_store_persistence_contract() {
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let mut store = SettingsEnvironmentStore::new(data.path().to_path_buf(), workspace.path());

        // Both scopes start empty, and neither inherits anything yet.
        assert!(matches!(
            EnvironmentStorePort::load(&mut store, EnvScope::Workspace),
            BackendEvent::EnvironmentLoaded { entries, inherited, .. }
                if entries.is_empty() && inherited.is_empty()
        ));

        // A global save lands in the per-user settings file.
        assert!(matches!(
            EnvironmentStorePort::save(
                &mut store,
                EnvScope::Global,
                vec![
                    EnvironmentEntry {
                        name: "GH_TOKEN".to_owned(),
                        value: "op://Private/GitHub/token".to_owned(),
                    },
                    // Unusable bindings are dropped rather than stored.
                    EnvironmentEntry {
                        name: "1BAD".to_owned(),
                        value: "x".to_owned(),
                    },
                ],
            ),
            BackendEvent::EnvironmentLoaded { scope, entries, inherited }
                if scope == EnvScope::Global
                    && entries.len() == 1
                    && entries[0].name == "GH_TOKEN"
                    && inherited.is_empty()
        ));

        // The workspace scope owns only its own bindings but reports the global
        // ones as inherited, so the editor can show what is already set.
        assert!(matches!(
            EnvironmentStorePort::save(
                &mut store,
                EnvScope::Workspace,
                vec![EnvironmentEntry {
                    name: "RUST_LOG".to_owned(),
                    value: "debug".to_owned(),
                }],
            ),
            BackendEvent::EnvironmentLoaded { entries, inherited, .. }
                if entries.len() == 1
                    && entries[0].name == "RUST_LOG"
                    && inherited.len() == 1
                    && inherited[0].name == "GH_TOKEN"
        ));

        // The writes landed in the two settings files, and the global save left
        // the rest of the settings file intact.
        assert_eq!(
            Storage::new(data.path().to_path_buf())
                .load_settings()
                .unwrap()
                .env
                .get("GH_TOKEN")
                .map(String::as_str),
            Some("op://Private/GitHub/token")
        );
        assert_eq!(
            WorkspaceSettingsStore::new(workspace.path())
                .load()
                .unwrap()
                .env
                .get("RUST_LOG")
                .map(String::as_str),
            Some("debug")
        );

        // An unreadable settings file fails safely: the editor keeps its values.
        std::fs::write(data.path().join("settings.json"), "{ broken").unwrap();
        assert!(matches!(
            EnvironmentStorePort::load(&mut store, EnvScope::Global),
            BackendEvent::EnvironmentError { .. }
        ));
        assert!(matches!(
            EnvironmentStorePort::save(&mut store, EnvScope::Global, Vec::new()),
            BackendEvent::EnvironmentError { .. }
        ));
    }

    #[test]
    fn workspace_state_loader_surfaces_a_malformed_state_file() {
        let workspace = tempfile::tempdir().unwrap();
        let state_dir = project_data_dir(workspace.path());
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("state.json"), "{ broken").unwrap();

        let error = load_workspace_state(workspace.path()).unwrap_err();
        assert!(error.to_string().contains("state.json"));
        let snapshot = LifecycleSnapshot {
            workspace_id: WorkspaceId::new(),
            root_worktree_id: usagi_core::domain::id::WorktreeId::new(),
            revision: 0,
            sessions: Vec::new(),
            agent_resumes: std::collections::BTreeMap::new(),
        };
        let workspace = Workspace::new("broken", workspace.path());
        assert!(session_snapshot_result("refresh", &snapshot, &workspace).is_err());
    }

    #[test]
    fn global_modal_mode_survives_a_new_tui_settings_port() {
        let temporary = tempfile::tempdir().unwrap();
        let storage = Storage::new(temporary.path());
        let mut first = PersistentSettingsPort {
            storage: Storage::new(temporary.path()),
            workspace: None,
        };
        let settings = Settings {
            modal_selection_mode: ModalSelectionMode::Prompt,
            ..Settings::default()
        };
        first.save(SettingsScope::Global, &settings).unwrap();
        let mut restarted = PersistentSettingsPort {
            storage,
            workspace: None,
        };
        assert_eq!(
            restarted
                .read(SettingsScope::Global)
                .unwrap()
                .modal_selection_mode,
            ModalSelectionMode::Prompt
        );
    }

    #[test]
    fn settings_scope_routing_projects_global_storage_failures() {
        let temporary = tempfile::tempdir().unwrap();
        let storage_file = temporary.path().join("not-a-directory");
        std::fs::write(&storage_file, "x").unwrap();
        let mut settings = PersistentSettingsPort {
            storage: Storage::new(&storage_file),
            workspace: None,
        };

        assert!(settings.read(SettingsScope::Global).is_err());
        assert!(
            settings
                .save(SettingsScope::Workspace, &Settings::default())
                .is_err()
        );
        assert!(
            settings
                .save(SettingsScope::Global, &Settings::default())
                .is_err()
        );
    }

    #[test]
    fn workspace_settings_keep_global_ui_values_and_durable_local_tool_values() {
        let temporary = tempfile::tempdir().unwrap();
        let global_dir = temporary.path().join("global");
        let workspace_a = temporary.path().join("a");
        let workspace_b = temporary.path().join("b");
        std::fs::create_dir_all(&workspace_a).unwrap();
        std::fs::create_dir_all(&workspace_b).unwrap();
        let global = Settings {
            theme: Theme::Dark,
            modal_selection_mode: ModalSelectionMode::Action,
            ..Settings::default()
        };
        Storage::new(&global_dir).save_settings(&global).unwrap();

        let mut port = PersistentSettingsPort {
            storage: Storage::new(&global_dir),
            workspace: None,
        };
        port.select_workspace(&workspace_a).unwrap();
        assert_eq!(port.read(SettingsScope::Workspace).unwrap(), global);
        let local_a = Settings {
            theme: Theme::Light,
            modal_selection_mode: ModalSelectionMode::Prompt,
            default_model: usagi_core::domain::settings::DefaultModel::Claude,
            issue_enabled: false,
            ..global.clone()
        };
        port.save(SettingsScope::Workspace, &local_a).unwrap();
        let effective_a = Settings {
            theme: Theme::Dark,
            modal_selection_mode: ModalSelectionMode::Action,
            default_model: usagi_core::domain::settings::DefaultModel::Claude,
            issue_enabled: false,
            ..global.clone()
        };

        let mut reopened = PersistentSettingsPort {
            storage: Storage::new(&global_dir),
            workspace: None,
        };
        reopened.select_workspace(&workspace_a).unwrap();
        assert_eq!(
            reopened.read(SettingsScope::Workspace).unwrap(),
            effective_a
        );
        let changed_global = Settings {
            theme: Theme::Light,
            modal_selection_mode: ModalSelectionMode::Prompt,
            ..Settings::default()
        };
        reopened
            .save(SettingsScope::Global, &changed_global)
            .unwrap();
        assert_eq!(
            reopened.read(SettingsScope::Workspace).unwrap(),
            Settings {
                default_model: usagi_core::domain::settings::DefaultModel::Claude,
                issue_enabled: false,
                ..changed_global.clone()
            }
        );
        reopened.select_workspace(&workspace_b).unwrap();
        assert_eq!(
            reopened.read(SettingsScope::Workspace).unwrap(),
            changed_global
        );
    }

    #[test]
    fn new_workspace_settings_copy_global_defaults_once() {
        let temporary = tempfile::tempdir().unwrap();
        let global_dir = temporary.path().join("global");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let initial = Settings {
            theme: Theme::Dark,
            modal_selection_mode: ModalSelectionMode::Prompt,
            default_model: usagi_core::domain::settings::DefaultModel::Claude,
            issue_enabled: false,
            memory_enabled: false,
            env: usagi_core::domain::settings::EnvBindings::new(),
        };
        let storage = Storage::new(&global_dir);
        storage.save_settings(&initial).unwrap();
        let loader = FsWorkspaceLoader { storage };

        loader.initialize_workspace_settings(&workspace).unwrap();
        let saved = WorkspaceSettingsStore::new(&workspace).load().unwrap();
        assert_eq!(saved, LocalSettings::from(&initial));

        loader.storage.save_settings(&Settings::default()).unwrap();
        loader.initialize_workspace_settings(&workspace).unwrap();
        assert_eq!(
            WorkspaceSettingsStore::new(&workspace).load().unwrap(),
            saved
        );
    }

    #[test]
    fn workspace_settings_unknown_values_defer_and_corrupt_files_error() {
        let temporary = tempfile::tempdir().unwrap();
        let global_dir = temporary.path().join("global");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let global = Settings {
            theme: Theme::Dark,
            modal_selection_mode: ModalSelectionMode::Prompt,
            ..Settings::default()
        };
        Storage::new(&global_dir).save_settings(&global).unwrap();
        let local_store = WorkspaceSettingsStore::new(&workspace);
        std::fs::create_dir_all(local_store.path().parent().unwrap()).unwrap();
        std::fs::write(
            local_store.path(),
            r#"{"version":1,"theme":"future","modal_selection_mode":"future"}"#,
        )
        .unwrap();
        let mut port = PersistentSettingsPort {
            storage: Storage::new(&global_dir),
            workspace: None,
        };
        port.select_workspace(&workspace).unwrap();
        assert_eq!(port.read(SettingsScope::Workspace).unwrap(), global);

        std::fs::write(local_store.path(), "{ broken").unwrap();
        assert!(port.read(SettingsScope::Workspace).is_err());
        assert_eq!(
            usagi_core::usecase::settings::read_for_workspace_entry(&mut port),
            global
        );
        let local = LocalSettings::default();
        assert_eq!(global.clone().with_local(&local), global);
    }

    #[test]
    fn production_backend_factory_preserves_terminal_arguments_and_completes_store_routes() {
        let temporary = tempfile::tempdir().unwrap();
        let workspace_id = WorkspaceId::new();
        let session_ids = vec![SessionId::new(), SessionId::new()];
        let session = |name: &str, display_name| SessionRecord {
            name: name.to_owned(),
            display_name,
            origin: SessionOrigin::Human,
            started_from: None,
            root: temporary.path().join(name),
            created_at: Utc::now(),
            last_active: None,
            notes: Scratchpad::default(),
            prs: Vec::new(),
        };
        let snapshot = WorkspaceSnapshot::with_runtime_ids(
            Workspace::new("demo", temporary.path()),
            WorkspaceState {
                sessions: vec![
                    session("alpha", Some("Alpha".to_owned())),
                    session("beta", None),
                ],
                ..Default::default()
            },
            workspace_id,
            session_ids,
        );
        let (host, actions) = ControllerHost::channel();
        let mut factory = ProductionBackendFactory;
        let mut composition = factory.create(&snapshot, host);
        let operation_id = OperationId::new();

        composition.backend.dispatch(Effect::OpenTerminal {
            target: Target::Root(workspace_id),
            operation_id,
            arguments: "new".to_owned(),
        });
        assert!(matches!(
            actions.recv().unwrap(),
            ControllerHostAction::OpenTerminal(request)
                if request.operation_id == operation_id && request.arguments == "new"
        ));

        composition.backend.dispatch(Effect::LoadNotes {
            target: Target::Root(workspace_id),
        });
        composition.backend.dispatch(Effect::LoadEnvironment {
            scope: EnvScope::Workspace,
        });
        composition.backend.dispatch(Effect::LoadPreview {
            target: Target::Root(workspace_id),
        });
        let completions = composition.backend.drain_events();
        assert!(matches!(
            completions.as_slice(),
            [
                usagi_tui::usecase::application::controller::AppEvent::Backend(
                    BackendEvent::NotesLoaded { .. }
                ),
                usagi_tui::usecase::application::controller::AppEvent::Backend(
                    BackendEvent::EnvironmentLoaded { .. }
                ),
                usagi_tui::usecase::application::controller::AppEvent::Backend(
                    BackendEvent::PreviewLoaded { .. }
                )
            ]
        ));
    }

    #[test]
    fn production_reducer_harness_covers_entry_and_new_success_failure_routes() {
        let workspace = WorkspaceId::new();
        let choice = EntryWorkspace::new(workspace, "demo");
        let mut entry = EntryState::new(vec![choice.clone()], vec![workspace]);
        assert!(update_entry(&mut entry, EntryEvent::ShowOpen).is_empty());
        assert!(matches!(
            update_entry(&mut entry, EntryEvent::OpenSingle(workspace)).as_slice(),
            [Effect::AttachWorkspace { workspace: selected }] if *selected == workspace
        ));

        let mut entry = EntryState::new(vec![choice], vec![workspace]);
        assert!(matches!(
            update_entry(&mut entry, EntryEvent::OpenRecent(workspace)).as_slice(),
            [Effect::AttachWorkspace { workspace: selected }] if *selected == workspace
        ));
        let mut entry = EntryState::new(Vec::new(), Vec::new());
        let _ = update_entry(&mut entry, EntryEvent::ShowOpen);
        assert!(update_entry(&mut entry, EntryEvent::Back).is_empty());

        let mut new = NewState::new(
            NewMode::Existing,
            NewForm {
                path: "/tmp/demo".to_owned(),
                name: "demo".to_owned(),
                ..Default::default()
            },
        );
        let effects = update_new(&mut new, NewEvent::Submit);
        let token = match effects.as_slice() {
            [Effect::RegisterWorkspace { token, .. }] => *token,
            other => panic!("unexpected new effect: {other:?}"),
        };
        assert!(
            update_new(
                &mut new,
                NewEvent::Result {
                    token,
                    result: Err(Notice::new("registration failed")),
                },
            )
            .is_empty()
        );
        assert!(matches!(
            update_new(&mut new, NewEvent::Retry).as_slice(),
            [Effect::RegisterWorkspace { .. }]
        ));

        let session = SessionId::new();
        let runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let projected = ProjectedSession {
            id: session,
            label: "demo".to_owned(),
            detail: "human".to_owned(),
            cwd: std::path::PathBuf::from("/tmp/demo"),
            last_modified: Utc::now(),
            has_notes: true,
            pr_summary: None,
            removing: false,
            agent_resume: None,
            lifecycle: usagi_core::domain::session_lifecycle::SessionLifecycle::Available,
            failure_summary: None,
        };
        let frame = runtime.render(
            24,
            80,
            "demo",
            "/tmp/demo",
            &[projected],
            None,
            &std::collections::BTreeMap::new(),
            None,
        );
        assert!(frame.join("\n").contains('✎'));
    }
}

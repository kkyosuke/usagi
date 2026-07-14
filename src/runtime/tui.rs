//! TUI 面へ実端末と filesystem を接続する composition adapter。

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

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
use usagi_core::domain::workspace::Workspace;
use usagi_core::infrastructure::error_log::ErrorLog;
use usagi_core::infrastructure::store::state::WorkspaceStateStore;
use usagi_core::infrastructure::store::workspace::Storage;
use usagi_core::usecase::client::{
    AgentLaunchIntent, ClientPolicy, DaemonClient, DaemonMetrics, DaemonReply, DaemonRequest,
    MetricsAction, SessionAction,
};
use usagi_core::usecase::settings::{SettingsPort, SettingsScope};
use usagi_core::usecase::workspace as workspace_usecase;
use usagi_tui::infrastructure::metrics::MetricsHook;
use usagi_tui::presentation::frame::{Frame, FrameRenderer};
use usagi_tui::presentation::views::config::{self, AvailableAgentModels, Config};
use usagi_tui::presentation::views::welcome::{self, Welcome};
use usagi_tui::presentation::views::workspace::{self, Workspace as WorkspaceView};
use usagi_tui::presentation::{
    self, AgentCommandPort, AgentCommandPortFactory, BannerScreenRunner, Exit, MetricsPort,
    MetricsPortFactory, SessionCommandPort, SessionCommandPortFactory, SessionCommandResult, Start,
    WorkspaceLoader, WorkspaceSnapshot,
};
use usagi_tui::usecase::application::{self, EntryScreen, Key, Terminal};
use usagi_tui::usecase::overview::SessionCommand;
use usagi_tui::usecase::terminal_input::{
    KeyCode, KeyEventKind, LiveInput, LiveInputClassifier, LiveInputOutput, RuntimeEvent,
};

use crate::tui_input::{CrosstermSource, EventPump, NoBackend};

/// Composition adapter for Overview's daemon-owned session lifecycle commands.
#[derive(Default)]
struct DaemonSessionCommandPort {
    last_revision: u64,
}

struct DaemonMetricsPort {
    last_sample: Option<Instant>,
    latest: Option<DaemonMetrics>,
}
impl DaemonMetricsPort {
    // Composition-only adapter: it constructs the real daemon client and uses
    // the monotonic clock. The presentation `MetricsPort` is covered with fakes.
    #[coverage(off)]
    const fn new() -> Self {
        Self {
            last_sample: None,
            latest: None,
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
}

struct DaemonMetricsPortFactory;

impl MetricsPortFactory for DaemonMetricsPortFactory {
    #[coverage(off)]
    fn create(&mut self) -> Box<dyn MetricsPort> {
        Box::new(DaemonMetricsPort::new())
    }
}

/// Root composition adapter for the only Agent launch authority: the daemon.
struct DaemonAgentCommandPort;

struct DaemonAgentCommandPortFactory;

impl AgentCommandPortFactory for DaemonAgentCommandPortFactory {
    #[coverage(off)]
    fn create(&mut self) -> Box<dyn AgentCommandPort> {
        Box::new(DaemonAgentCommandPort)
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
            .map_err(|error| format!("daemon request failed: {error}"))?;
        let message = match reply {
            DaemonReply::Accepted {
                operation_id,
                revision,
                ..
            } => format!("accepted operation {operation_id} (revision {revision})"),
            DaemonReply::Ok(value) => {
                let snapshot = lifecycle_snapshot(&value)?;
                if snapshot.revision < self.last_revision {
                    return Ok(SessionCommandResult::message(
                        "ignored stale daemon snapshot",
                    ));
                }
                self.last_revision = snapshot.revision;
                return Ok(session_snapshot_result(
                    "daemon snapshot refreshed",
                    snapshot,
                    workspace,
                ));
            }
        };
        let snapshot = request_lifecycle_snapshot()?;
        if snapshot.revision < self.last_revision {
            return Ok(SessionCommandResult::message(
                "ignored stale daemon snapshot",
            ));
        }
        self.last_revision = snapshot.revision;
        Ok(session_snapshot_result(message, snapshot, workspace))
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
        .map_err(|error| format!("daemon request failed: {error}"))?
    {
        DaemonReply::Ok(value) => lifecycle_snapshot(&value),
        DaemonReply::Accepted { .. } => {
            Err("daemon returned an invalid lifecycle snapshot response".to_owned())
        }
    }
}

struct CrosstermTerminal {
    out: std::io::Stdout,
    input: EventPump<CrosstermSource, NoBackend<()>>,
    input_started: Instant,
    renderer: FrameRenderer,
    /// live-terminal `Ctrl-O` prefix の SSoT。leader を保持して follow-up を
    /// [`Key::Live`] へ翻訳する。`Ctrl-O`・`Ctrl-^` 以外は passthrough として従来の
    /// `Key` マッピングに委ねるため、live terminal への passthrough を壊さない。
    live_input: LiveInputClassifier,
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
                    // Exit chords stay outside the live-prefix classifier: Ctrl-Q ends
                    // the workspace, while Ctrl-C only closes the TUI.
                    if let LiveInput::Key(key) = &input
                        && key.modifiers.control
                    {
                        if key.code == KeyCode::Char('c') {
                            return Ok(Key::Quit);
                        }
                        if key.code == KeyCode::Char('q') {
                            return Ok(Key::CtrlQ);
                        }
                    }
                    let now = self.input_started.elapsed();
                    match self.live_input.classify(now, input.clone()) {
                        // A resolved `Ctrl-O` prefix action drives the live runtime.
                        LiveInputOutput::Action(action) => return Ok(Key::Live(action)),
                        // Leader pending, unknown follow-up, or key release: keep reading.
                        LiveInputOutput::Swallowed => {}
                        // Everything else is a normal key; preserve the prior mapping so
                        // non-prefix keys and future PTY passthrough are unchanged.
                        LiveInputOutput::Passthrough(_) => return Ok(passthrough_key(&input)),
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
}

/// Map a non-prefix live input to the management `Key` vocabulary. The classifier
/// has already reserved the `Ctrl-O` prefix, so this preserves the prior mapping
/// for every other key and text/paste payload.
#[coverage(off)]
fn passthrough_key(input: &LiveInput) -> Key {
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
        LiveInput::Raw(_) | LiveInput::Text(_) | LiveInput::Paste(_) => return Key::Other,
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
        let mut state = WorkspaceStateStore::new(&workspace.path)
            .load()
            .unwrap_or_default()
            .unwrap_or_default();
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
                    Box::new(DaemonAgentCommandPort),
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
    use super::{PersistentSettingsPort, Start, load_screen_graph_data, passthrough_key};
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
        assert_eq!(passthrough_key(&key), Key::Char('\u{1}'));
    }

    #[test]
    fn repeat_enter_reaches_the_closeup_action_handler() {
        let key = LiveInput::Key(KeyEvent::new(
            KeyCode::Enter,
            Modifiers::default(),
            KeyEventKind::Repeat,
        ));

        assert_eq!(passthrough_key(&key), Key::Enter);
    }

    #[test]
    fn released_enter_does_not_repeat_the_closeup_action() {
        let key = LiveInput::Key(KeyEvent::new(
            KeyCode::Enter,
            Modifiers::default(),
            KeyEventKind::Release,
        ));

        assert_eq!(passthrough_key(&key), Key::Other);
    }

    #[test]
    fn raw_return_reaches_the_closeup_action_handler() {
        for input in [
            LiveInput::Raw(b"\r".to_vec()),
            LiveInput::Text("\n".to_owned()),
        ] {
            assert_eq!(passthrough_key(&input), Key::Enter);
        }
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

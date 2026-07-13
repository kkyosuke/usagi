//! TUI 面へ実端末と filesystem を接続する composition adapter。

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::Utc;
use crossterm::cursor;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::terminal::{
    self, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{execute, queue};
use usagi_core::domain::AppInfo;
use usagi_core::domain::recent::Recent;
use usagi_core::domain::settings::Settings;
use usagi_core::domain::workspace::Workspace;
use usagi_core::infrastructure::store::state::WorkspaceStateStore;
use usagi_core::infrastructure::store::workspace::Storage;
use usagi_core::usecase::settings::{SettingsPort, SettingsScope};
use usagi_core::usecase::workspace as workspace_usecase;
use usagi_tui::presentation::frame::{Frame, FrameRenderer};
use usagi_tui::presentation::views::config::{self, Config};
use usagi_tui::presentation::views::welcome::{self, Welcome};
use usagi_tui::presentation::views::workspace::{self, Workspace as WorkspaceView};
use usagi_tui::presentation::{
    self, BannerScreenRunner, Exit, Start, WorkspaceLoader, WorkspaceSnapshot,
};
use usagi_tui::usecase::application::{self, EntryScreen, Key, Terminal};
use usagi_tui::usecase::terminal_input::{KeyCode, LiveInput, RuntimeEvent};

use crate::tui_input::{CrosstermSource, EventPump, NoBackend};

struct CrosstermTerminal {
    out: std::io::Stdout,
    input: EventPump<CrosstermSource, NoBackend<()>>,
    input_started: Instant,
    renderer: FrameRenderer,
}

#[derive(Default)]
struct VolatileSettingsPort {
    global: Settings,
    workspace: Settings,
}

impl SettingsPort for VolatileSettingsPort {
    #[coverage(off)]
    fn read(&mut self, scope: SettingsScope) -> std::io::Result<Settings> {
        Ok(match scope {
            SettingsScope::Global => self.global.clone(),
            SettingsScope::Workspace => self.workspace.clone(),
        })
    }

    #[coverage(off)]
    fn save(&mut self, scope: SettingsScope, settings: &Settings) -> std::io::Result<()> {
        match scope {
            SettingsScope::Global => self.global = settings.clone(),
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
    fn read_key(&mut self) -> std::io::Result<Key> {
        loop {
            match self.input.next(self.input_started.elapsed())? {
                RuntimeEvent::Input(LiveInput::Key(key)) => {
                    if key.modifiers.control && key.code == KeyCode::Char('c') {
                        return Ok(Key::Quit);
                    }
                    if !matches!(
                        key.kind,
                        usagi_tui::usecase::terminal_input::KeyEventKind::Press
                    ) {
                        return Ok(Key::Other);
                    }
                    return Ok(match key.code {
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
                    });
                }
                RuntimeEvent::Resize { .. } => {
                    self.renderer.reset_surface();
                    return Ok(Key::Other);
                }
                RuntimeEvent::Input(
                    LiveInput::Text(_) | LiveInput::Paste(_) | LiveInput::Raw(_),
                )
                | RuntimeEvent::Backend(()) => return Ok(Key::Other),
                RuntimeEvent::Tick => {}
            }
        }
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
        let state = WorkspaceStateStore::new(&workspace.path)
            .load()
            .unwrap_or_default()
            .unwrap_or_default();
        Ok(WorkspaceSnapshot::new(workspace, state))
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
        cursor::Hide
    ) {
        let _ = execute!(
            setup,
            cursor::Show,
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
    };
    let result = run(&mut terminal);
    let mut teardown = std::io::stdout();
    let _ = execute!(
        teardown,
        cursor::Show,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
    result
}

#[coverage(off)]
fn launch_screen_graph(out: &mut dyn Write, start: Start) -> std::io::Result<()> {
    let now = Utc::now();
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        let storage = Storage::open_default().map_err(io_error)?;
        let (workspaces, recent) = load_screen_graph_data(&storage, start)?;
        let mut loader = FsWorkspaceLoader { storage };
        let mut settings = VolatileSettingsPort::default();
        run_in_terminal(|terminal| {
            presentation::run_with_settings(
                terminal,
                workspaces,
                recent,
                now,
                start,
                &mut loader,
                &mut settings,
            )
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
                let mut settings = VolatileSettingsPort::default();
                config::render(0, 0, &Config::load(&mut settings))
            }
        };
        for line in frame {
            writeln!(out, "{line}")?;
        }
    }
    Ok(())
}

#[coverage(off)]
fn launch_workspace(out: &mut dyn Write, path: &Path) -> std::io::Result<()> {
    let mut loader = FsWorkspaceLoader::open_default()?;
    let snapshot = loader.open(path)?;
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        run_in_terminal(|terminal| presentation::run_workspace(terminal, snapshot))?;
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
    use super::{Start, load_screen_graph_data};
    use usagi_core::infrastructure::store::workspace::Storage;

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
}

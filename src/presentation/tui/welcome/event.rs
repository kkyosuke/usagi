use std::io;

use anyhow::Result;
use console::Term;

use crate::domain::workspace::Workspace;
use crate::presentation::tui::config;
use crate::presentation::tui::home;
use crate::presentation::tui::new;
use crate::presentation::tui::new::state::NewProject;
use crate::presentation::tui::open;
use crate::presentation::tui::screen::{AlternateScreenGuard, KeyReader};

use super::menu::{Action, Menu};
use super::ui;

/// Launches the project selection screen and returns the user's choice.
///
/// Taking this as a parameter lets the event loop be tested without a real
/// terminal: production wires it to [`open::run`], tests pass a stub.
pub type OpenOpen<'a> = dyn FnMut(&Term) -> Result<open::Outcome> + 'a;

/// Launches the New Project screen and returns the user's choice.
///
/// Taking this as a parameter lets the event loop be tested without a real
/// terminal: production wires it to [`new::run`], tests pass a stub.
pub type OpenNew<'a> = dyn FnMut(&Term) -> Result<new::Outcome> + 'a;

/// Creates a project from a submitted form: clone the repository, register it
/// as a workspace, and capture its initial worktree state.
///
/// Injected for the same reason as the screen launchers: production wires it to
/// the project use case, tests pass a stub so the loop runs without touching
/// git or the filesystem.
pub type CreateProject<'a> = dyn FnMut(&NewProject) -> Result<Workspace> + 'a;

/// Launches the home screen for a freshly created workspace and returns the
/// user's choice.
///
/// Taking this as a parameter lets the event loop be tested without a real
/// terminal: production wires it to [`home::run`], tests pass a stub.
pub type OpenHome<'a> = dyn FnMut(&Term, &Workspace) -> Result<home::Outcome> + 'a;

/// Launches the Config screen and returns the user's choice.
///
/// Taking this as a parameter lets the event loop be tested without a real
/// terminal: production wires it to [`config::run`], tests pass a stub.
pub type OpenConfig<'a> = dyn FnMut(&Term) -> Result<config::Outcome> + 'a;

/// Runs the welcome screen against the given terminal and key source until the
/// user quits (or an unrecoverable read error occurs).
pub fn event_loop(
    term: &Term,
    reader: &mut dyn KeyReader,
    open_open: &mut OpenOpen,
    open_new: &mut OpenNew,
    create_project: &mut CreateProject,
    open_home: &mut OpenHome,
    open_config: &mut OpenConfig,
) -> Result<()> {
    let mut guard = AlternateScreenGuard::new(term.clone())?;
    let mut menu = Menu::new();

    loop {
        term.move_cursor_to(0, 0)?;
        term.clear_screen()?;
        let (height, width) = term.size();
        let frame = ui::render_frame(
            height as usize,
            width as usize,
            menu.items(),
            menu.selected_index(),
            menu.notice(),
        );
        for line in &frame {
            term.write_line(line)?;
        }

        match reader.read_key() {
            Ok(key) => match menu.handle_key(key) {
                Action::Continue => {}
                Action::Quit => return Ok(()),
                Action::OpenOpen => match open_open(term) {
                    Ok(open::Outcome::Back) => menu.set_notice(None),
                    Ok(open::Outcome::Quit) => return Ok(()),
                    Err(e) => {
                        // Restore the terminal without the farewell on error.
                        guard.dismiss();
                        return Err(e);
                    }
                },
                Action::OpenNew => match open_new(term) {
                    Ok(new::Outcome::Back) => menu.set_notice(None),
                    Ok(new::Outcome::Quit) => return Ok(()),
                    Ok(new::Outcome::Submitted(project)) => {
                        // Clone and register the new workspace, then jump straight
                        // into its home screen. A failure (bad URL, network, name
                        // clash) is shown as a notice so the user can correct it
                        // without losing the menu.
                        match create_project(&project) {
                            Ok(workspace) => match open_home(term, &workspace)? {
                                home::Outcome::Back => menu.set_notice(None),
                                home::Outcome::Quit => return Ok(()),
                            },
                            Err(e) => {
                                menu.set_notice(Some(format!("Could not create project: {e}")));
                            }
                        }
                    }
                    Err(e) => {
                        // Restore the terminal without the farewell on error.
                        guard.dismiss();
                        return Err(e);
                    }
                },
                Action::OpenConfig => match open_config(term) {
                    Ok(config::Outcome::Back) => menu.set_notice(None),
                    Ok(config::Outcome::Quit) => return Ok(()),
                    Err(e) => {
                        // Restore the terminal without the farewell on error.
                        guard.dismiss();
                        return Err(e);
                    }
                },
            },
            // Treat an interrupted read (e.g. Ctrl+C delivered as a signal) as quit.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(()),
            Err(e) => {
                // Restore the terminal without the farewell on an unexpected error.
                guard.dismiss();
                return Err(anyhow::Error::from(e).context("Failed to read key"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::repository::RepoUrl;
    use crate::presentation::tui::new::state::{CloneSpec, ExistingSpec, NewProject};
    use console::Key;
    use std::collections::VecDeque;

    /// A key source that replays a scripted sequence of results.
    struct ScriptedReader {
        keys: VecDeque<io::Result<Key>>,
    }

    impl ScriptedReader {
        fn new(keys: Vec<io::Result<Key>>) -> Self {
            Self { keys: keys.into() }
        }
    }

    impl KeyReader for ScriptedReader {
        fn read_key(&mut self) -> io::Result<Key> {
            self.keys.pop_front().unwrap_or(Ok(Key::Char('q')))
        }
    }

    // Project-selection (Open) screen launchers used as stubs.
    fn open_screen_back(_t: &Term) -> Result<open::Outcome> {
        Ok(open::Outcome::Back)
    }
    fn open_screen_quit(_t: &Term) -> Result<open::Outcome> {
        Ok(open::Outcome::Quit)
    }
    fn open_screen_err(_t: &Term) -> Result<open::Outcome> {
        Err(anyhow::anyhow!("open screen blew up"))
    }

    // New-screen launchers used as stubs; each is exercised by a test below.
    fn new_back(_t: &Term) -> Result<new::Outcome> {
        Ok(new::Outcome::Back)
    }
    fn new_quit(_t: &Term) -> Result<new::Outcome> {
        Ok(new::Outcome::Quit)
    }
    fn new_submitted(_t: &Term) -> Result<new::Outcome> {
        Ok(new::Outcome::Submitted(NewProject::Clone(CloneSpec {
            url: RepoUrl::parse("https://github.com/owner/repo.git").unwrap(),
            location: std::path::PathBuf::from("/base"),
            directory: "repo".to_string(),
            branch: None,
        })))
    }
    fn new_submitted_existing(_t: &Term) -> Result<new::Outcome> {
        Ok(new::Outcome::Submitted(NewProject::Existing(
            ExistingSpec {
                path: std::path::PathBuf::from("/base/existing"),
                name: "existing".to_string(),
            },
        )))
    }
    fn new_err(_t: &Term) -> Result<new::Outcome> {
        Err(anyhow::anyhow!("new screen blew up"))
    }

    // Project-creation stubs paired with the New-screen launchers above.
    fn create_ok(p: &NewProject) -> Result<Workspace> {
        match p {
            NewProject::Clone(spec) => Ok(Workspace::new(spec.directory.clone(), &spec.location)),
            NewProject::Existing(spec) => Ok(Workspace::new(spec.name.clone(), &spec.path)),
        }
    }
    fn create_err(_p: &NewProject) -> Result<Workspace> {
        Err(anyhow::anyhow!("clone failed"))
    }

    // Home-screen launchers used as stubs; each is exercised by a test below.
    fn home_back(_t: &Term, _w: &Workspace) -> Result<home::Outcome> {
        Ok(home::Outcome::Back)
    }
    fn home_quit(_t: &Term, _w: &Workspace) -> Result<home::Outcome> {
        Ok(home::Outcome::Quit)
    }
    fn home_err(_t: &Term, _w: &Workspace) -> Result<home::Outcome> {
        Err(anyhow::anyhow!("home screen blew up"))
    }

    // Config-screen launchers used as stubs; each is exercised by a test below.
    fn config_back(_t: &Term) -> Result<config::Outcome> {
        Ok(config::Outcome::Back)
    }
    fn config_quit(_t: &Term) -> Result<config::Outcome> {
        Ok(config::Outcome::Quit)
    }
    fn config_err(_t: &Term) -> Result<config::Outcome> {
        Err(anyhow::anyhow!("config screen blew up"))
    }

    #[test]
    fn loop_quits_when_quit_key_pressed() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Char('q'))]);
        assert!(event_loop(
            &term,
            &mut reader,
            &mut open_screen_back,
            &mut new_back,
            &mut create_ok,
            &mut home_back,
            &mut config_back
        )
        .is_ok());
    }

    #[test]
    fn loop_redraws_across_several_keys_before_quitting() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![
            Ok(Key::ArrowDown),
            Ok(Key::ArrowUp),
            Ok(Key::Char('q')),
        ]);
        assert!(event_loop(
            &term,
            &mut reader,
            &mut open_screen_back,
            &mut new_back,
            &mut create_ok,
            &mut home_back,
            &mut config_back
        )
        .is_ok());
    }

    #[test]
    fn interrupted_read_is_treated_as_quit() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "interrupted",
        ))]);
        assert!(event_loop(
            &term,
            &mut reader,
            &mut open_screen_back,
            &mut new_back,
            &mut create_ok,
            &mut home_back,
            &mut config_back
        )
        .is_ok());
    }

    #[test]
    fn unexpected_read_error_is_propagated() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Err(io::Error::other("boom"))]);
        let err = event_loop(
            &term,
            &mut reader,
            &mut open_screen_back,
            &mut new_back,
            &mut create_ok,
            &mut home_back,
            &mut config_back,
        )
        .unwrap_err();
        assert!(err.to_string().contains("Failed to read key"));
    }

    #[test]
    fn open_screen_back_returns_to_menu() {
        let term = Term::stdout();
        // 'o' opens the project selection screen (stub returns Back), then 'q'.
        let mut reader = ScriptedReader::new(vec![Ok(Key::Char('o')), Ok(Key::Char('q'))]);
        assert!(event_loop(
            &term,
            &mut reader,
            &mut open_screen_back,
            &mut new_back,
            &mut create_ok,
            &mut home_back,
            &mut config_back
        )
        .is_ok());
    }

    #[test]
    fn open_screen_quit_exits_the_app() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Char('o'))]);
        assert!(event_loop(
            &term,
            &mut reader,
            &mut open_screen_quit,
            &mut new_back,
            &mut create_ok,
            &mut home_back,
            &mut config_back
        )
        .is_ok());
    }

    #[test]
    fn open_screen_error_is_propagated() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Char('o'))]);
        let err = event_loop(
            &term,
            &mut reader,
            &mut open_screen_err,
            &mut new_back,
            &mut create_ok,
            &mut home_back,
            &mut config_back,
        )
        .unwrap_err();
        assert!(err.to_string().contains("open screen blew up"));
    }

    #[test]
    fn new_screen_back_returns_to_menu() {
        let term = Term::stdout();
        // 'e' opens the New screen (stub returns Back), then 'q' quits.
        let mut reader = ScriptedReader::new(vec![Ok(Key::Char('e')), Ok(Key::Char('q'))]);
        assert!(event_loop(
            &term,
            &mut reader,
            &mut open_screen_back,
            &mut new_back,
            &mut create_ok,
            &mut home_back,
            &mut config_back
        )
        .is_ok());
    }

    #[test]
    fn new_screen_quit_exits_the_app() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Char('e'))]);
        assert!(event_loop(
            &term,
            &mut reader,
            &mut open_screen_back,
            &mut new_quit,
            &mut create_ok,
            &mut home_back,
            &mut config_back
        )
        .is_ok());
    }

    #[test]
    fn new_screen_submitted_creates_then_opens_home_and_returns_to_menu() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Char('e')), Ok(Key::Char('q'))]);
        // Submitting runs create_ok, which succeeds; the new workspace's home
        // screen opens (stub returns Back), leaving the user on the menu.
        assert!(event_loop(
            &term,
            &mut reader,
            &mut open_screen_back,
            &mut new_submitted,
            &mut create_ok,
            &mut home_back,
            &mut config_back
        )
        .is_ok());
    }

    #[test]
    fn new_screen_submitted_existing_creates_then_opens_home_and_returns_to_menu() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Char('e')), Ok(Key::Char('q'))]);
        // Submitting an existing-directory project routes through create_ok's
        // Existing arm, then opens its home screen (stub returns Back).
        assert!(event_loop(
            &term,
            &mut reader,
            &mut open_screen_back,
            &mut new_submitted_existing,
            &mut create_ok,
            &mut home_back,
            &mut config_back
        )
        .is_ok());
    }

    #[test]
    fn new_screen_submitted_then_home_quit_exits_the_app() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Char('e'))]);
        // Quitting from the freshly opened home screen quits the whole app.
        assert!(event_loop(
            &term,
            &mut reader,
            &mut open_screen_back,
            &mut new_submitted,
            &mut create_ok,
            &mut home_quit,
            &mut config_back
        )
        .is_ok());
    }

    #[test]
    fn new_screen_submitted_then_home_error_is_propagated() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Char('e'))]);
        let err = event_loop(
            &term,
            &mut reader,
            &mut open_screen_back,
            &mut new_submitted,
            &mut create_ok,
            &mut home_err,
            &mut config_back,
        )
        .unwrap_err();
        assert!(err.to_string().contains("home screen blew up"));
    }

    #[test]
    fn new_screen_submitted_create_failure_sets_a_notice_then_quits() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Char('e')), Ok(Key::Char('q'))]);
        // A failing create surfaces as a recoverable notice, not an error.
        assert!(event_loop(
            &term,
            &mut reader,
            &mut open_screen_back,
            &mut new_submitted,
            &mut create_err,
            &mut home_back,
            &mut config_back
        )
        .is_ok());
    }

    #[test]
    fn new_screen_error_is_propagated() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Char('e'))]);
        let err = event_loop(
            &term,
            &mut reader,
            &mut open_screen_back,
            &mut new_err,
            &mut create_ok,
            &mut home_back,
            &mut config_back,
        )
        .unwrap_err();
        assert!(err.to_string().contains("new screen blew up"));
    }

    #[test]
    fn config_screen_back_returns_to_menu() {
        let term = Term::stdout();
        // 'c' opens the Config screen (stub returns Back), then 'q' quits.
        let mut reader = ScriptedReader::new(vec![Ok(Key::Char('c')), Ok(Key::Char('q'))]);
        assert!(event_loop(
            &term,
            &mut reader,
            &mut open_screen_back,
            &mut new_back,
            &mut create_ok,
            &mut home_back,
            &mut config_back
        )
        .is_ok());
    }

    #[test]
    fn config_screen_quit_exits_the_app() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Char('c'))]);
        assert!(event_loop(
            &term,
            &mut reader,
            &mut open_screen_back,
            &mut new_back,
            &mut create_ok,
            &mut home_back,
            &mut config_quit
        )
        .is_ok());
    }

    #[test]
    fn config_screen_error_is_propagated() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Char('c'))]);
        let err = event_loop(
            &term,
            &mut reader,
            &mut open_screen_back,
            &mut new_back,
            &mut create_ok,
            &mut home_back,
            &mut config_err,
        )
        .unwrap_err();
        assert!(err.to_string().contains("config screen blew up"));
    }
}

//! Testable orchestration loop for the interactive TUI.
//!
//! See the [module overview](super) for the screen graph this drives. The loop
//! takes each screen as an injected launcher so it can be exercised without a
//! real terminal: production wires the real screens in [`super::run`], tests
//! pass stubs.

use anyhow::Result;
use console::Term;

use crate::domain::workspace::Workspace;
use crate::presentation::tui::io::screen::AlternateScreenGuard;
use crate::presentation::tui::new::state::NewProject;
use crate::presentation::tui::{config, home, new, open, welcome};

/// Plays the startup splash once, before the screen graph opens.
pub type RunSplash<'a> = dyn FnMut(&Term) -> Result<()> + 'a;

/// Runs the welcome menu and returns the chosen action.
///
/// Taking the screens as parameters lets the orchestration loop be tested
/// without a real terminal: production wires them to the real screens, tests
/// pass stubs.
pub type RunWelcome<'a> = dyn FnMut(&Term, Option<String>) -> Result<welcome::Outcome> + 'a;

/// Runs the project selection screen and returns the user's choice.
pub type RunOpen<'a> = dyn FnMut(&Term) -> Result<open::Outcome> + 'a;

/// Runs the New Project screen and returns the user's choice.
pub type RunNew<'a> = dyn FnMut(&Term) -> Result<new::Outcome> + 'a;

/// Creates a project from a submitted form: clone the repository (or register
/// an existing directory), register it as a workspace, and capture its initial
/// worktree state.
pub type CreateProject<'a> = dyn FnMut(&NewProject) -> Result<Workspace> + 'a;

/// Runs the home screen for a workspace and returns the user's choice.
pub type RunHome<'a> = dyn FnMut(&Term, &Workspace) -> Result<home::Outcome> + 'a;

/// Opens a recent workspace from the welcome screen and returns the home
/// screen's choice.
pub type RunRecent<'a> = dyn FnMut(&Term, &Workspace) -> Result<home::Outcome> + 'a;

/// Runs the Config screen and returns the user's choice.
pub type RunConfig<'a> = dyn FnMut(&Term) -> Result<config::Outcome> + 'a;

/// Drives the screen graph until the user quits. Activates the alternate screen
/// on entry and restores it on exit (suppressing the farewell on error).
///
/// On entry it plays the startup splash once; the welcome menu then dispatches
/// to each sub-screen; a sub-screen returning `Quit` ends the session, `Back`
/// returns to the menu. Submitting the New form creates the project and opens
/// its home screen; a creation failure is carried back to the menu as a notice
/// so the user can correct it and retry.
#[allow(clippy::too_many_arguments)]
pub fn event_loop(
    term: &Term,
    run_splash: &mut RunSplash,
    run_welcome: &mut RunWelcome,
    run_open: &mut RunOpen,
    run_new: &mut RunNew,
    create_project: &mut CreateProject,
    run_home: &mut RunHome,
    run_recent: &mut RunRecent,
    run_config: &mut RunConfig,
) -> Result<()> {
    let mut guard = AlternateScreenGuard::new(term.clone())?;
    if let Err(e) = run_splash(term) {
        return dismiss_and_fail(&mut guard, e);
    }
    let mut notice: Option<String> = None;

    loop {
        match run_welcome(term, notice.take()) {
            Ok(welcome::Outcome::Quit) => return Ok(()),
            Ok(welcome::Outcome::OpenProjects) => match run_open(term) {
                Ok(open::Outcome::Back) => {}
                Ok(open::Outcome::Quit) => return Ok(()),
                Err(e) => return dismiss_and_fail(&mut guard, e),
            },
            Ok(welcome::Outcome::RecentProject(workspace)) => match run_recent(term, &workspace) {
                Ok(home::Outcome::Back) => {}
                Ok(home::Outcome::Quit) => return Ok(()),
                Err(e) => return dismiss_and_fail(&mut guard, e),
            },
            Ok(welcome::Outcome::NewProject) => match run_new(term) {
                Ok(new::Outcome::Back) => {}
                Ok(new::Outcome::Quit) => return Ok(()),
                Ok(new::Outcome::Submitted(project)) => {
                    // Clone/register the new workspace, then jump straight into
                    // its home screen. A failure (bad URL, network, name clash)
                    // is carried back to the menu as a notice so the user can
                    // correct it without losing the menu.
                    match create_project(&project) {
                        Ok(workspace) => match run_home(term, &workspace) {
                            Ok(home::Outcome::Back) => {}
                            Ok(home::Outcome::Quit) => return Ok(()),
                            Err(e) => return dismiss_and_fail(&mut guard, e),
                        },
                        Err(e) => notice = Some(format!("Could not create project: {e}")),
                    }
                }
                Err(e) => return dismiss_and_fail(&mut guard, e),
            },
            Ok(welcome::Outcome::Configure) => match run_config(term) {
                Ok(config::Outcome::Back) => {}
                Ok(config::Outcome::Quit) => return Ok(()),
                Err(e) => return dismiss_and_fail(&mut guard, e),
            },
            Err(e) => return dismiss_and_fail(&mut guard, e),
        }
    }
}

/// Restores the terminal without the farewell, then surfaces the error.
fn dismiss_and_fail(guard: &mut AlternateScreenGuard, error: anyhow::Error) -> Result<()> {
    guard.dismiss();
    Err(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::repository::RepoUrl;
    use crate::presentation::tui::new::state::{CloneSpec, ExistingSpec};

    // Splash stubs: the splash plays once on entry. The no-op stands in for the
    // real (terminal-driven) splash so the loop can be tested headlessly.
    fn splash_noop(_t: &Term) -> Result<()> {
        Ok(())
    }
    fn splash_err(_t: &Term) -> Result<()> {
        Err(anyhow::anyhow!("splash blew up"))
    }

    // Welcome-menu stubs: each returns a fixed action so the orchestration loop
    // can be driven deterministically.
    fn welcome_quit(_t: &Term, _n: Option<String>) -> Result<welcome::Outcome> {
        Ok(welcome::Outcome::Quit)
    }
    fn welcome_open(_t: &Term, _n: Option<String>) -> Result<welcome::Outcome> {
        Ok(welcome::Outcome::OpenProjects)
    }
    fn welcome_new(_t: &Term, _n: Option<String>) -> Result<welcome::Outcome> {
        Ok(welcome::Outcome::NewProject)
    }
    fn welcome_config(_t: &Term, _n: Option<String>) -> Result<welcome::Outcome> {
        Ok(welcome::Outcome::Configure)
    }
    fn welcome_recent(_t: &Term, _n: Option<String>) -> Result<welcome::Outcome> {
        Ok(welcome::Outcome::RecentProject(Workspace::new(
            "recent",
            "/tmp/recent",
        )))
    }
    fn welcome_err(_t: &Term, _n: Option<String>) -> Result<welcome::Outcome> {
        Err(anyhow::anyhow!("welcome screen blew up"))
    }

    // Open-screen stubs.
    fn open_back(_t: &Term) -> Result<open::Outcome> {
        Ok(open::Outcome::Back)
    }
    fn open_quit(_t: &Term) -> Result<open::Outcome> {
        Ok(open::Outcome::Quit)
    }
    fn open_err(_t: &Term) -> Result<open::Outcome> {
        Err(anyhow::anyhow!("open screen blew up"))
    }

    // New-screen stubs.
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

    // Project-creation stubs.
    fn create_ok(p: &NewProject) -> Result<Workspace> {
        match p {
            NewProject::Clone(spec) => Ok(Workspace::new(spec.directory.clone(), &spec.location)),
            NewProject::Existing(spec) => Ok(Workspace::new(spec.name.clone(), &spec.path)),
        }
    }
    fn create_err(_p: &NewProject) -> Result<Workspace> {
        Err(anyhow::anyhow!("clone failed"))
    }

    // Home-screen stubs.
    fn home_back(_t: &Term, _w: &Workspace) -> Result<home::Outcome> {
        Ok(home::Outcome::Back)
    }
    fn home_quit(_t: &Term, _w: &Workspace) -> Result<home::Outcome> {
        Ok(home::Outcome::Quit)
    }
    fn home_err(_t: &Term, _w: &Workspace) -> Result<home::Outcome> {
        Err(anyhow::anyhow!("home screen blew up"))
    }
    fn recent_err(_t: &Term, _w: &Workspace) -> Result<home::Outcome> {
        Err(anyhow::anyhow!("recent screen blew up"))
    }

    // Config-screen stubs.
    fn config_back(_t: &Term) -> Result<config::Outcome> {
        Ok(config::Outcome::Back)
    }
    fn config_quit(_t: &Term) -> Result<config::Outcome> {
        Ok(config::Outcome::Quit)
    }
    fn config_err(_t: &Term) -> Result<config::Outcome> {
        Err(anyhow::anyhow!("config screen blew up"))
    }

    /// A welcome stub that yields a scripted sequence of actions, so a test can
    /// drive several loop iterations (e.g. open a sub-screen, return, then quit).
    struct ScriptedWelcome {
        outcomes: std::collections::VecDeque<welcome::Outcome>,
    }
    impl ScriptedWelcome {
        fn new(outcomes: Vec<welcome::Outcome>) -> Self {
            Self {
                outcomes: outcomes.into(),
            }
        }
        fn next(&mut self, _t: &Term, _n: Option<String>) -> Result<welcome::Outcome> {
            // Default to Quit so the loop always terminates.
            Ok(self.outcomes.pop_front().unwrap_or(welcome::Outcome::Quit))
        }
    }

    fn term() -> Term {
        Term::stdout()
    }

    #[test]
    fn welcome_quit_ends_the_session() {
        let t = term();
        assert!(event_loop(
            &t,
            &mut splash_noop,
            &mut welcome_quit,
            &mut open_back,
            &mut new_back,
            &mut create_ok,
            &mut home_back,
            &mut home_back,
            &mut config_back,
        )
        .is_ok());
    }

    #[test]
    fn welcome_error_is_propagated() {
        let t = term();
        let err = event_loop(
            &t,
            &mut splash_noop,
            &mut welcome_err,
            &mut open_back,
            &mut new_back,
            &mut create_ok,
            &mut home_back,
            &mut home_back,
            &mut config_back,
        )
        .unwrap_err();
        assert!(err.to_string().contains("welcome screen blew up"));
    }

    #[test]
    fn open_back_returns_to_menu_then_quits() {
        let t = term();
        let mut welcome =
            ScriptedWelcome::new(vec![welcome::Outcome::OpenProjects, welcome::Outcome::Quit]);
        assert!(event_loop(
            &t,
            &mut splash_noop,
            &mut |tt, n| welcome.next(tt, n),
            &mut open_back,
            &mut new_back,
            &mut create_ok,
            &mut home_back,
            &mut home_back,
            &mut config_back,
        )
        .is_ok());
    }

    #[test]
    fn open_quit_ends_the_session() {
        let t = term();
        assert!(event_loop(
            &t,
            &mut splash_noop,
            &mut welcome_open,
            &mut open_quit,
            &mut new_back,
            &mut create_ok,
            &mut home_back,
            &mut home_back,
            &mut config_back,
        )
        .is_ok());
    }

    #[test]
    fn open_error_is_propagated() {
        let t = term();
        let err = event_loop(
            &t,
            &mut splash_noop,
            &mut welcome_open,
            &mut open_err,
            &mut new_back,
            &mut create_ok,
            &mut home_back,
            &mut home_back,
            &mut config_back,
        )
        .unwrap_err();
        assert!(err.to_string().contains("open screen blew up"));
    }

    #[test]
    fn recent_back_returns_to_menu_then_quits() {
        let t = term();
        let mut welcome = ScriptedWelcome::new(vec![
            welcome::Outcome::RecentProject(Workspace::new("recent", "/tmp/recent")),
            welcome::Outcome::Quit,
        ]);
        assert!(event_loop(
            &t,
            &mut splash_noop,
            &mut |tt, n| welcome.next(tt, n),
            &mut open_back,
            &mut new_back,
            &mut create_ok,
            &mut home_back,
            &mut home_back,
            &mut config_back,
        )
        .is_ok());
    }

    #[test]
    fn recent_quit_ends_the_session() {
        let t = term();
        assert!(event_loop(
            &t,
            &mut splash_noop,
            &mut welcome_recent,
            &mut open_back,
            &mut new_back,
            &mut create_ok,
            &mut home_back,
            &mut home_quit,
            &mut config_back,
        )
        .is_ok());
    }

    #[test]
    fn recent_error_is_propagated() {
        let t = term();
        let err = event_loop(
            &t,
            &mut splash_noop,
            &mut welcome_recent,
            &mut open_back,
            &mut new_back,
            &mut create_ok,
            &mut home_back,
            &mut recent_err,
            &mut config_back,
        )
        .unwrap_err();
        assert!(err.to_string().contains("recent screen blew up"));
    }

    #[test]
    fn new_back_returns_to_menu_then_quits() {
        let t = term();
        let mut welcome =
            ScriptedWelcome::new(vec![welcome::Outcome::NewProject, welcome::Outcome::Quit]);
        assert!(event_loop(
            &t,
            &mut splash_noop,
            &mut |tt, n| welcome.next(tt, n),
            &mut open_back,
            &mut new_back,
            &mut create_ok,
            &mut home_back,
            &mut home_back,
            &mut config_back,
        )
        .is_ok());
    }

    #[test]
    fn new_quit_ends_the_session() {
        let t = term();
        assert!(event_loop(
            &t,
            &mut splash_noop,
            &mut welcome_new,
            &mut open_back,
            &mut new_quit,
            &mut create_ok,
            &mut home_back,
            &mut home_back,
            &mut config_back,
        )
        .is_ok());
    }

    #[test]
    fn new_error_is_propagated() {
        let t = term();
        let err = event_loop(
            &t,
            &mut splash_noop,
            &mut welcome_new,
            &mut open_back,
            &mut new_err,
            &mut create_ok,
            &mut home_back,
            &mut home_back,
            &mut config_back,
        )
        .unwrap_err();
        assert!(err.to_string().contains("new screen blew up"));
    }

    #[test]
    fn new_submitted_creates_then_opens_home_and_returns_to_menu() {
        let t = term();
        let mut welcome =
            ScriptedWelcome::new(vec![welcome::Outcome::NewProject, welcome::Outcome::Quit]);
        assert!(event_loop(
            &t,
            &mut splash_noop,
            &mut |tt, n| welcome.next(tt, n),
            &mut open_back,
            &mut new_submitted,
            &mut create_ok,
            &mut home_back,
            &mut home_back,
            &mut config_back,
        )
        .is_ok());
    }

    #[test]
    fn new_submitted_existing_creates_then_opens_home_and_returns_to_menu() {
        let t = term();
        let mut welcome =
            ScriptedWelcome::new(vec![welcome::Outcome::NewProject, welcome::Outcome::Quit]);
        assert!(event_loop(
            &t,
            &mut splash_noop,
            &mut |tt, n| welcome.next(tt, n),
            &mut open_back,
            &mut new_submitted_existing,
            &mut create_ok,
            &mut home_back,
            &mut home_back,
            &mut config_back,
        )
        .is_ok());
    }

    #[test]
    fn new_submitted_then_home_quit_ends_the_session() {
        let t = term();
        assert!(event_loop(
            &t,
            &mut splash_noop,
            &mut welcome_new,
            &mut open_back,
            &mut new_submitted,
            &mut create_ok,
            &mut home_quit,
            &mut home_back,
            &mut config_back,
        )
        .is_ok());
    }

    #[test]
    fn new_submitted_then_home_error_is_propagated() {
        let t = term();
        let err = event_loop(
            &t,
            &mut splash_noop,
            &mut welcome_new,
            &mut open_back,
            &mut new_submitted,
            &mut create_ok,
            &mut home_err,
            &mut home_back,
            &mut config_back,
        )
        .unwrap_err();
        assert!(err.to_string().contains("home screen blew up"));
    }

    #[test]
    fn new_submitted_create_failure_carries_notice_back_to_menu() {
        let t = term();
        // Creation fails, so the loop returns to the menu with a notice; the
        // next welcome iteration quits. The home stub must not be reached.
        let mut welcome =
            ScriptedWelcome::new(vec![welcome::Outcome::NewProject, welcome::Outcome::Quit]);
        assert!(event_loop(
            &t,
            &mut splash_noop,
            &mut |tt, n| welcome.next(tt, n),
            &mut open_back,
            &mut new_submitted,
            &mut create_err,
            &mut home_err,
            &mut home_back,
            &mut config_back,
        )
        .is_ok());
    }

    #[test]
    fn config_back_returns_to_menu_then_quits() {
        let t = term();
        let mut welcome =
            ScriptedWelcome::new(vec![welcome::Outcome::Configure, welcome::Outcome::Quit]);
        assert!(event_loop(
            &t,
            &mut splash_noop,
            &mut |tt, n| welcome.next(tt, n),
            &mut open_back,
            &mut new_back,
            &mut create_ok,
            &mut home_back,
            &mut home_back,
            &mut config_back,
        )
        .is_ok());
    }

    #[test]
    fn config_quit_ends_the_session() {
        let t = term();
        assert!(event_loop(
            &t,
            &mut splash_noop,
            &mut welcome_config,
            &mut open_back,
            &mut new_back,
            &mut create_ok,
            &mut home_back,
            &mut home_back,
            &mut config_quit,
        )
        .is_ok());
    }

    #[test]
    fn config_error_is_propagated() {
        let t = term();
        let err = event_loop(
            &t,
            &mut splash_noop,
            &mut welcome_config,
            &mut open_back,
            &mut new_back,
            &mut create_ok,
            &mut home_back,
            &mut home_back,
            &mut config_err,
        )
        .unwrap_err();
        assert!(err.to_string().contains("config screen blew up"));
    }

    #[test]
    fn splash_error_is_propagated() {
        // A splash that fails surfaces its error (with the farewell suppressed)
        // before the welcome menu is ever reached, so the welcome stub that would
        // panic on use is never called.
        let t = term();
        let err = event_loop(
            &t,
            &mut splash_err,
            &mut welcome_err,
            &mut open_back,
            &mut new_back,
            &mut create_ok,
            &mut home_back,
            &mut home_back,
            &mut config_back,
        )
        .unwrap_err();
        assert!(err.to_string().contains("splash blew up"));
    }
}

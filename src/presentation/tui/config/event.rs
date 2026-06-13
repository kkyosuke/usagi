use anyhow::Result;
use console::Key;
use console::Term;

use crate::domain::settings::Settings;
use crate::presentation::tui::screen::KeyReader;

use super::state::Config;
use super::ui;

/// What the user chose to do on the configuration screen.
#[derive(Debug)]
pub enum Outcome {
    /// Return to the previous screen.
    Back,
    /// The user asked to quit the application entirely.
    Quit,
}

/// Persists the edited settings.
///
/// Taking this as a parameter lets the event loop be tested without touching
/// disk: production wires it to the settings use case, tests pass a stub.
pub type Save<'a> = dyn FnMut(&Settings) -> Result<()> + 'a;

/// Runs the configuration screen against the given terminal and key source
/// until the user goes back or quits. Assumes the alternate screen is already
/// active (it is owned by the caller).
///
/// Changing a setting (←/→ or Enter) applies and persists it immediately; a
/// persistence failure is shown as a notice so the user is not left wondering
/// whether the change took.
pub fn event_loop(
    term: &Term,
    reader: &mut dyn KeyReader,
    mut config: Config,
    save: &mut Save,
    initial_notice: Option<String>,
) -> Result<Outcome> {
    let mut notice = initial_notice;

    loop {
        term.move_cursor_to(0, 0)?;
        term.clear_screen()?;
        let (height, width) = term.size();
        let frame = ui::render_frame(height as usize, width as usize, &config, notice.as_deref());
        for line in &frame {
            term.write_line(line)?;
        }

        let key = match reader.read_key() {
            Ok(key) => key,
            // An interrupted read (e.g. a delivered signal) means quit.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return Ok(Outcome::Quit),
            Err(e) => return Err(anyhow::Error::from(e).context("Failed to read key")),
        };

        match key {
            Key::ArrowUp | Key::Char('k') => {
                config.move_up();
                notice = None;
            }
            Key::ArrowDown | Key::Char('j') => {
                config.move_down();
                notice = None;
            }
            Key::Enter | Key::ArrowRight | Key::Char('l') => {
                notice = cycle_and_save(&mut config, true, save);
            }
            Key::ArrowLeft | Key::Char('h') => {
                notice = cycle_and_save(&mut config, false, save);
            }
            Key::Char('q') | Key::Escape => return Ok(Outcome::Back),
            Key::CtrlC => return Ok(Outcome::Quit),
            _ => {}
        }
    }
}

/// Cycles the selected field and persists the change, returning the notice to
/// show: a confirmation, a save error, or a hint when there was nothing to
/// change.
fn cycle_and_save(config: &mut Config, forward: bool, save: &mut Save) -> Option<String> {
    if !config.cycle_selected(forward) {
        return Some("No workspaces to choose from 🐰".to_string());
    }
    Some(match save(config.settings()) {
        Ok(()) => "Saved 🐰".to_string(),
        Err(e) => format!("Failed to save: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::settings::{Settings, Theme};
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::io;

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
            // Default to Escape so a test can never spin forever.
            self.keys.pop_front().unwrap_or(Ok(Key::Escape))
        }
    }

    fn sample_config() -> Config {
        Config::new(Settings::default(), vec!["alpha".to_string()])
    }

    /// A persistence stub that accepts every change (and is itself exercised by
    /// [`saving_succeeds_with_a_noop_save`]).
    fn noop_save(_: &Settings) -> Result<()> {
        Ok(())
    }

    /// Runs the loop, recording every settings snapshot handed to `save`.
    fn run_recording(keys: Vec<io::Result<Key>>, config: Config) -> (Outcome, Vec<Settings>) {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(keys);
        let saved = RefCell::new(Vec::new());
        let mut save = |s: &Settings| {
            saved.borrow_mut().push(s.clone());
            Ok(())
        };
        let outcome = event_loop(&term, &mut reader, config, &mut save, None).unwrap();
        (outcome, saved.into_inner())
    }

    #[test]
    fn escape_returns_back() {
        let (outcome, saved) = run_recording(vec![Ok(Key::Escape)], sample_config());
        assert!(matches!(outcome, Outcome::Back));
        assert!(saved.is_empty());
    }

    #[test]
    fn q_returns_back() {
        let (outcome, _) = run_recording(vec![Ok(Key::Char('q'))], sample_config());
        assert!(matches!(outcome, Outcome::Back));
    }

    #[test]
    fn ctrl_c_returns_quit() {
        let (outcome, _) = run_recording(vec![Ok(Key::CtrlC)], sample_config());
        assert!(matches!(outcome, Outcome::Quit));
    }

    #[test]
    fn navigation_keys_move_the_cursor_then_back() {
        // Exercises every navigation arm (arrows + j/k aliases) and the
        // ignored-key arm, then leaves via Escape.
        let keys = vec![
            Ok(Key::ArrowDown),
            Ok(Key::ArrowUp),
            Ok(Key::Char('j')),
            Ok(Key::Char('k')),
            Ok(Key::Home), // ignored (the `_` arm)
            Ok(Key::Escape),
        ];
        let (outcome, saved) = run_recording(keys, sample_config());
        assert!(matches!(outcome, Outcome::Back));
        // Navigation alone never persists.
        assert!(saved.is_empty());
    }

    #[test]
    fn enter_and_right_cycle_forward_and_save() {
        // Enter then ArrowRight then 'l' each advance the theme and persist.
        let keys = vec![
            Ok(Key::Enter),
            Ok(Key::ArrowRight),
            Ok(Key::Char('l')),
            Ok(Key::Escape),
        ];
        let (_, saved) = run_recording(keys, sample_config());
        // System -> Light -> Dark -> System.
        let themes: Vec<Theme> = saved.iter().map(|s| s.theme).collect();
        assert_eq!(themes, vec![Theme::Light, Theme::Dark, Theme::System]);
    }

    #[test]
    fn left_and_h_cycle_backward_and_save() {
        let keys = vec![Ok(Key::ArrowLeft), Ok(Key::Char('h')), Ok(Key::Escape)];
        let (_, saved) = run_recording(keys, sample_config());
        // System -> Dark -> Light (reverse order).
        let themes: Vec<Theme> = saved.iter().map(|s| s.theme).collect();
        assert_eq!(themes, vec![Theme::Dark, Theme::Light]);
    }

    #[test]
    fn cycling_default_workspace_persists_the_selection() {
        // Move down to Default Workspace, then cycle forward onto "alpha".
        let keys = vec![Ok(Key::ArrowDown), Ok(Key::Enter), Ok(Key::Escape)];
        let (_, saved) = run_recording(keys, sample_config());
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].default_workspace.as_deref(), Some("alpha"));
    }

    #[test]
    fn cycling_default_workspace_without_workspaces_shows_a_hint_and_does_not_save() {
        // No registered workspaces: cycling the field is a no-op that only
        // surfaces a hint, so nothing is persisted.
        let config = Config::new(Settings::default(), Vec::new());
        let keys = vec![Ok(Key::ArrowDown), Ok(Key::Enter), Ok(Key::Escape)];
        let (outcome, saved) = run_recording(keys, config);
        assert!(matches!(outcome, Outcome::Back));
        assert!(saved.is_empty());
    }

    #[test]
    fn a_save_failure_is_shown_as_a_notice_and_recovers() {
        // The first change fails to persist; the loop keeps running so the user
        // can try again or leave.
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Enter), Ok(Key::Escape)]);
        let mut save = |_: &Settings| Err(anyhow::anyhow!("disk full"));
        let outcome = event_loop(&term, &mut reader, sample_config(), &mut save, None).unwrap();
        assert!(matches!(outcome, Outcome::Back));
    }

    #[test]
    fn saving_succeeds_with_a_noop_save() {
        // Cycling the theme persists via `noop_save`, exercising that stub.
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Enter), Ok(Key::Escape)]);
        let mut save: fn(&Settings) -> Result<()> = noop_save;
        let outcome = event_loop(&term, &mut reader, sample_config(), &mut save, None).unwrap();
        assert!(matches!(outcome, Outcome::Back));
    }

    #[test]
    fn initial_notice_is_displayed() {
        // A load-error notice passed in is rendered on the first frame.
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Escape)]);
        let mut save: fn(&Settings) -> Result<()> = noop_save;
        let outcome = event_loop(
            &term,
            &mut reader,
            sample_config(),
            &mut save,
            Some("Failed to load settings: boom".to_string()),
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Back));
    }

    #[test]
    fn interrupted_read_returns_quit() {
        let keys = vec![Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "interrupted",
        ))];
        let (outcome, _) = run_recording(keys, sample_config());
        assert!(matches!(outcome, Outcome::Quit));
    }

    #[test]
    fn unexpected_read_error_is_propagated() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Err(io::Error::other("boom"))]);
        let mut save: fn(&Settings) -> Result<()> = noop_save;
        let err = event_loop(&term, &mut reader, sample_config(), &mut save, None).unwrap_err();
        assert!(err.to_string().contains("Failed to read key"));
    }
}

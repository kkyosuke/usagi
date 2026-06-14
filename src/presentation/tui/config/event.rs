use anyhow::Result;
use console::Key;
use console::Term;

use crate::domain::settings::{LocalSettings, Settings};
use crate::presentation::tui::screen::{FramePainter, KeyReader};

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

/// Persists the edited settings: the global [`Settings`] plus, when the screen
/// has a project context, that project's [`LocalSettings`] overrides.
///
/// Taking this as a parameter lets the event loop be tested without touching
/// disk: production wires it to the settings use case, tests pass a stub.
pub type Save<'a> = dyn FnMut(&Settings, Option<&LocalSettings>) -> Result<()> + 'a;

/// Runs the configuration screen against the given terminal and key source
/// until the user goes back or quits. Assumes the alternate screen is already
/// active (it is owned by the caller).
///
/// Changing a setting (←/→, or Enter on a field) edits it in memory only — the
/// row is flagged as changed but nothing touches disk. The edits are written
/// only when the user moves to the Save button and presses Enter; a persistence
/// failure is shown as a notice so the user is not left wondering whether the
/// change took.
pub fn event_loop(
    term: &Term,
    reader: &mut dyn KeyReader,
    mut config: Config,
    save: &mut Save,
    initial_notice: Option<String>,
) -> Result<Outcome> {
    let mut notice = initial_notice;
    let mut painter = FramePainter::new();

    loop {
        let (height, width) = term.size();
        let frame = ui::render_frame(height as usize, width as usize, &config, notice.as_deref());
        painter.paint(term, frame)?;

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
            Key::ArrowRight | Key::Char('l') => {
                notice = change_field(&mut config, true);
            }
            Key::ArrowLeft | Key::Char('h') => {
                notice = change_field(&mut config, false);
            }
            Key::Enter => {
                // Enter saves on the Save button, and otherwise advances the
                // focused field — a convenient alias for →.
                notice = if config.is_save_selected() {
                    save_changes(&mut config, save)
                } else {
                    change_field(&mut config, true)
                };
            }
            Key::Char('q') | Key::Escape => return Ok(Outcome::Back),
            Key::CtrlC => return Ok(Outcome::Quit),
            _ => {}
        }
    }
}

/// Cycles the focused field's value (in memory only), returning a hint when
/// there was nothing to change and clearing the notice otherwise. A no-op on
/// the Save button, where ←/→ have nothing to cycle.
fn change_field(config: &mut Config, forward: bool) -> Option<String> {
    if config.is_save_selected() {
        return None;
    }
    if config.cycle_selected(forward) {
        None
    } else {
        Some("No workspaces to choose from 🐰".to_string())
    }
}

/// Persists the edits when there are any, returning the notice to show: a
/// confirmation, a save error, or a hint when there is nothing to save.
fn save_changes(config: &mut Config, save: &mut Save) -> Option<String> {
    if !config.is_dirty() {
        return Some("No changes to save 🐰".to_string());
    }
    Some(match save(config.settings(), config.local()) {
        Ok(()) => {
            config.mark_saved();
            "Saved 🐰".to_string()
        }
        Err(e) => format!("Failed to save: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::settings::{AgentCli, LocalSettings, Settings, Theme};
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
    fn noop_save(_: &Settings, _: Option<&LocalSettings>) -> Result<()> {
        Ok(())
    }

    /// Runs the loop, recording every settings snapshot handed to `save`.
    fn run_recording(keys: Vec<io::Result<Key>>, config: Config) -> (Outcome, Vec<Settings>) {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(keys);
        let saved = RefCell::new(Vec::new());
        let mut save = |s: &Settings, _: Option<&LocalSettings>| {
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
    fn arrows_edit_the_focused_field_without_saving() {
        // ←/→ and their h/l aliases all edit the focused field in memory only —
        // nothing is persisted until the Save button is pressed.
        let keys = vec![
            Ok(Key::ArrowRight),
            Ok(Key::Char('l')),
            Ok(Key::ArrowLeft),
            Ok(Key::Char('h')),
            Ok(Key::Escape),
        ];
        let (outcome, saved) = run_recording(keys, sample_config());
        assert!(matches!(outcome, Outcome::Back));
        assert!(saved.is_empty());
    }

    #[test]
    fn enter_on_a_field_cycles_it_forward_then_the_save_button_persists() {
        // Enter on the Theme field advances it (System -> Light); Up wraps onto
        // the Save button, where Enter writes the edit exactly once.
        let keys = vec![
            Ok(Key::Enter),
            Ok(Key::ArrowUp),
            Ok(Key::Enter),
            Ok(Key::Escape),
        ];
        let (_, saved) = run_recording(keys, sample_config());
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].theme, Theme::Light);
    }

    #[test]
    fn the_save_button_persists_once_and_clears_the_dirty_state() {
        // Edit the theme, save it, then press Save again with nothing pending:
        // the second press finds no changes and does not persist again.
        let keys = vec![
            Ok(Key::ArrowRight), // System -> Light
            Ok(Key::ArrowUp),    // onto the Save button
            Ok(Key::Enter),      // saves Light
            Ok(Key::Enter),      // nothing left to save
            Ok(Key::Escape),
        ];
        let (_, saved) = run_recording(keys, sample_config());
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].theme, Theme::Light);
    }

    #[test]
    fn enter_on_the_save_button_with_no_edits_does_not_persist() {
        let keys = vec![Ok(Key::ArrowUp), Ok(Key::Enter), Ok(Key::Escape)];
        let (outcome, saved) = run_recording(keys, sample_config());
        assert!(matches!(outcome, Outcome::Back));
        assert!(saved.is_empty());
    }

    #[test]
    fn arrows_on_the_save_button_do_nothing() {
        // ←/→ have no value to cycle on the Save button, so they are no-ops.
        let keys = vec![
            Ok(Key::ArrowUp), // onto the Save button
            Ok(Key::ArrowRight),
            Ok(Key::ArrowLeft),
            Ok(Key::Escape),
        ];
        let (outcome, saved) = run_recording(keys, sample_config());
        assert!(matches!(outcome, Outcome::Back));
        assert!(saved.is_empty());
    }

    #[test]
    fn cycling_default_workspace_persists_when_saved() {
        // Move down to Default Workspace, cycle onto "alpha", then save.
        let keys = vec![
            Ok(Key::ArrowDown),  // Default Workspace
            Ok(Key::ArrowRight), // -> alpha
            Ok(Key::ArrowDown),  // Notifications
            Ok(Key::ArrowDown),  // Agent CLI
            Ok(Key::ArrowDown),  // Save button
            Ok(Key::Enter),      // saves
            Ok(Key::Escape),
        ];
        let (_, saved) = run_recording(keys, sample_config());
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].default_workspace.as_deref(), Some("alpha"));
    }

    #[test]
    fn cycling_default_workspace_without_workspaces_shows_a_hint_and_does_not_save() {
        // No registered workspaces: cycling the field is a no-op that only
        // surfaces a hint, and there is nothing to save.
        let config = Config::new(Settings::default(), Vec::new());
        let keys = vec![Ok(Key::ArrowDown), Ok(Key::ArrowRight), Ok(Key::Escape)];
        let (outcome, saved) = run_recording(keys, config);
        assert!(matches!(outcome, Outcome::Back));
        assert!(saved.is_empty());
    }

    #[test]
    fn a_save_failure_is_shown_as_a_notice_and_recovers() {
        // The save fails to persist; the loop keeps running so the user can try
        // again or leave.
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![
            Ok(Key::ArrowRight), // edit the theme so there is something to save
            Ok(Key::ArrowUp),    // onto the Save button
            Ok(Key::Enter),      // attempt the save
            Ok(Key::Escape),
        ]);
        let mut save = |_: &Settings, _: Option<&LocalSettings>| Err(anyhow::anyhow!("disk full"));
        let outcome = event_loop(&term, &mut reader, sample_config(), &mut save, None).unwrap();
        assert!(matches!(outcome, Outcome::Back));
    }

    #[test]
    fn saving_succeeds_with_a_noop_save() {
        // Editing then saving persists via `noop_save`, exercising that stub.
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![
            Ok(Key::ArrowRight),
            Ok(Key::ArrowUp),
            Ok(Key::Enter),
            Ok(Key::Escape),
        ]);
        let mut save: fn(&Settings, Option<&LocalSettings>) -> Result<()> = noop_save;
        let outcome = event_loop(&term, &mut reader, sample_config(), &mut save, None).unwrap();
        assert!(matches!(outcome, Outcome::Back));
    }

    #[test]
    fn initial_notice_is_displayed() {
        // A load-error notice passed in is rendered on the first frame.
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Escape)]);
        let mut save: fn(&Settings, Option<&LocalSettings>) -> Result<()> = noop_save;
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
    fn saving_a_local_override_passes_it_to_save() {
        // Open in the local scope, set a local agent-CLI override, then save: the
        // local settings reach the save callback. The local scope shows only the
        // three override rows, with Agent CLI selected from the start.
        let term = Term::stdout();
        let config = Config::workspace(Settings::default(), LocalSettings::default());
        let keys = vec![
            Ok(Key::ArrowRight), // Agent CLI override: Global -> Claude
            Ok(Key::ArrowDown),  // Notifications
            Ok(Key::ArrowDown),  // Default Branch
            Ok(Key::ArrowDown),  // Save button
            Ok(Key::Enter),      // save
            Ok(Key::Escape),
        ];

        let mut reader = ScriptedReader::new(keys);
        let captured: RefCell<Option<LocalSettings>> = RefCell::new(None);
        let mut save = |_: &Settings, local: Option<&LocalSettings>| {
            *captured.borrow_mut() = local.cloned();
            Ok(())
        };
        let outcome = event_loop(&term, &mut reader, config, &mut save, None).unwrap();
        assert!(matches!(outcome, Outcome::Back));
        let local = captured.into_inner().expect("save received local settings");
        assert_eq!(local.agent_cli, Some(AgentCli::Claude));
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
        let mut save: fn(&Settings, Option<&LocalSettings>) -> Result<()> = noop_save;
        let err = event_loop(&term, &mut reader, sample_config(), &mut save, None).unwrap_err();
        assert!(err.to_string().contains("Failed to read key"));
    }
}

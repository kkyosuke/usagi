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

/// Provisions the `ollama` runtime, taking the sudo password entered in the
/// install modal so the install can elevate non-interactively. Injected like
/// [`Save`] so the event loop is testable without shelling out: production wires
/// it to the `local_llm` use case (which runs the install on a background thread
/// behind a spinner), tests pass a stub.
pub type InstallRuntime<'a> = dyn FnMut(&str) -> Result<()> + 'a;

/// Pulls a model into the installed runtime (the model-picker's "install on
/// select" path). Injected like [`InstallRuntime`]; `ollama pull` is
/// unprivileged, so it takes only the model name. Production runs it on a
/// background thread behind the same spinner.
pub type PullModel<'a> = dyn FnMut(&str) -> Result<()> + 'a;

/// Runs the configuration screen against the given terminal and key source
/// until the user goes back or quits. Assumes the alternate screen is already
/// active (it is owned by the caller).
///
/// Changing a setting (←/→, or Enter on a field) edits it in memory only — the
/// row is flagged as changed but nothing touches disk. The edits are written
/// only when the user moves to the Save button and presses Enter; a persistence
/// failure is shown as a notice so the user is not left wondering whether the
/// change took. The local LLM rows are the exception: while the runtime is
/// missing the Local LLM row is an "Install" action — Space or Enter opens a
/// modal that collects the sudo password, and confirming runs `install_runtime`
/// (provisioning is an action, not a saved setting); the cursor then drops onto
/// the model row. Once installed, the model row opens a picker (Space/Enter)
/// listing the offered models with their install state; choosing an installed
/// one adopts it, choosing an uninstalled one runs `pull_model` first.
pub fn event_loop(
    term: &Term,
    reader: &mut dyn KeyReader,
    mut config: Config,
    save: &mut Save,
    install_runtime: &mut InstallRuntime,
    pull_model: &mut PullModel,
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

        // While the install modal is open it captures every key: printable
        // characters build the sudo password, Enter confirms (running the
        // install), and Esc cancels.
        if config.install_modal().is_some() {
            match key {
                Key::Enter => {
                    notice = run_install(&mut config, install_runtime);
                    // The install painted its own spinner frames over ours, so
                    // forget the remembered frame and repaint the screen fully.
                    painter.reset();
                }
                Key::Backspace => config.install_modal_backspace(),
                Key::Del => config.install_modal_delete_forward(),
                // ←/→/Home/End move the caret so the password can be edited
                // mid-string, not only at the end.
                Key::ArrowLeft => config.install_modal_cursor_left(),
                Key::ArrowRight => config.install_modal_cursor_right(),
                Key::Home => config.install_modal_cursor_home(),
                Key::End => config.install_modal_cursor_end(),
                Key::Char(c) => config.install_modal_push(c),
                Key::Escape => config.close_install_modal(),
                Key::CtrlC => return Ok(Outcome::Quit),
                _ => {}
            }
            continue;
        }

        // The model picker likewise captures every key: ↑/↓ move the cursor,
        // Enter adopts the highlighted model (pulling it first if it is not yet
        // present), and Esc cancels.
        if config.model_modal().is_some() {
            match key {
                Key::ArrowUp | Key::Char('k') => config.model_modal_up(),
                Key::ArrowDown | Key::Char('j') => config.model_modal_down(),
                Key::Enter => {
                    notice = run_model_select(&mut config, pull_model);
                    // A pull paints its own spinner frames; repaint fully.
                    painter.reset();
                }
                Key::Escape => config.close_model_modal(),
                Key::CtrlC => return Ok(Outcome::Quit),
                _ => {}
            }
            continue;
        }

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
                notice = activate_field(&mut config, true);
            }
            Key::ArrowLeft | Key::Char('h') => {
                notice = activate_field(&mut config, false);
            }
            Key::Char(' ') => {
                // Space opens the install modal on the Local LLM install action,
                // or the model picker on the active model row; each is a no-op
                // off its own row, so calling both is safe.
                config.open_install_modal();
                config.open_model_modal();
                notice = None;
            }
            Key::Enter => {
                // Enter saves on the Save button, opens the install modal on the
                // Local LLM install action, opens the model picker on the active
                // model row, and otherwise advances the focused field (a
                // convenient alias for →).
                if config.is_save_selected() {
                    notice = save_changes(&mut config, save);
                } else if config.local_llm_needs_install() {
                    config.open_install_modal();
                    notice = None;
                } else if config.model_row_active() {
                    config.open_model_modal();
                    notice = None;
                } else {
                    notice = activate_field(&mut config, true);
                }
            }
            Key::Char('q') | Key::Escape => return Ok(Outcome::Back),
            Key::CtrlC => return Ok(Outcome::Quit),
            _ => {}
        }
    }
}

/// Handles an arrow press on the focused field. The Local LLM install action
/// and the active model row have no value to cycle (both are driven by their
/// modals), so arrows are a no-op there; otherwise the field's value is cycled.
fn activate_field(config: &mut Config, forward: bool) -> Option<String> {
    if config.local_llm_needs_install() || config.model_row_active() {
        None
    } else {
        change_field(config, forward)
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
        Some("Nothing to choose from 🐰".to_string())
    }
}

/// Runs the runtime install with the sudo password from the modal, closes the
/// modal, and returns the notice to show. On success the Local LLM row flips
/// from "Install" to an on/off toggle (now on) and the cursor drops onto the
/// model row so a model can be chosen.
fn run_install(config: &mut Config, install_runtime: &mut InstallRuntime) -> Option<String> {
    let password = config.install_modal_password().unwrap_or_default();
    let result = install_runtime(&password);
    config.close_install_modal();
    Some(match result {
        Ok(()) => {
            config.mark_ollama_installed();
            config.focus_model_row();
            "Installed ollama 🐰".to_string()
        }
        Err(e) => format!("Install failed: {e}"),
    })
}

/// Adopts the model highlighted in the picker, closes the modal, and returns
/// the notice to show. An already-installed model is adopted directly; an
/// uninstalled one is pulled first via `pull_model` (a failed pull leaves the
/// model unchanged and is surfaced as a notice).
fn run_model_select(config: &mut Config, pull_model: &mut PullModel) -> Option<String> {
    let model = config.model_modal_selection()?.to_string();
    if config.model_modal_selection_installed() {
        config.select_model(&model);
        config.close_model_modal();
        return Some(format!("Using {model} 🐰"));
    }
    let result = pull_model(&model);
    config.close_model_modal();
    Some(match result {
        Ok(()) => {
            config.mark_model_installed(&model);
            format!("Installed {model} 🐰")
        }
        Err(e) => format!("Install failed: {e}"),
    })
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

    /// A runtime-install stub that succeeds without doing anything (its body is
    /// exercised by the install tests below).
    fn ok_install(_: &str) -> Result<()> {
        Ok(())
    }

    /// A model-pull stub that succeeds without doing anything.
    fn ok_pull(_: &str) -> Result<()> {
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
        let mut install: fn(&str) -> Result<()> = ok_install;
        let mut pull: fn(&str) -> Result<()> = ok_pull;
        let outcome = event_loop(
            &term,
            &mut reader,
            config,
            &mut save,
            &mut install,
            &mut pull,
            None,
        )
        .unwrap();
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
            Ok(Key::ArrowDown),  // Session Action UI
            Ok(Key::ArrowDown),  // Local LLM
            Ok(Key::ArrowDown),  // Local LLM Model
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
        let mut install: fn(&str) -> Result<()> = ok_install;
        let mut pull: fn(&str) -> Result<()> = ok_pull;
        let outcome = event_loop(
            &term,
            &mut reader,
            sample_config(),
            &mut save,
            &mut install,
            &mut pull,
            None,
        )
        .unwrap();
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
        let mut install: fn(&str) -> Result<()> = ok_install;
        let mut pull: fn(&str) -> Result<()> = ok_pull;
        let outcome = event_loop(
            &term,
            &mut reader,
            sample_config(),
            &mut save,
            &mut install,
            &mut pull,
            None,
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Back));
    }

    #[test]
    fn initial_notice_is_displayed() {
        // A load-error notice passed in is rendered on the first frame.
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Escape)]);
        let mut save: fn(&Settings, Option<&LocalSettings>) -> Result<()> = noop_save;
        let mut install: fn(&str) -> Result<()> = ok_install;
        let mut pull: fn(&str) -> Result<()> = ok_pull;
        let outcome = event_loop(
            &term,
            &mut reader,
            sample_config(),
            &mut save,
            &mut install,
            &mut pull,
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
        let config = Config::workspace(Settings::default(), LocalSettings::default(), Vec::new());
        let keys = vec![
            Ok(Key::ArrowRight), // Agent CLI override: Global -> Claude
            Ok(Key::ArrowDown),  // Notifications
            Ok(Key::ArrowDown),  // Default Branch
            Ok(Key::ArrowDown),  // Branch Source
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
        let mut install: fn(&str) -> Result<()> = ok_install;
        let mut pull: fn(&str) -> Result<()> = ok_pull;
        let outcome = event_loop(
            &term,
            &mut reader,
            config,
            &mut save,
            &mut install,
            &mut pull,
            None,
        )
        .unwrap();
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
        let mut install: fn(&str) -> Result<()> = ok_install;
        let mut pull: fn(&str) -> Result<()> = ok_pull;
        let err = event_loop(
            &term,
            &mut reader,
            sample_config(),
            &mut save,
            &mut install,
            &mut pull,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("Failed to read key"));
    }

    // --- local LLM install action -----------------------------------------

    /// Drives the loop with recording install/pull stubs, returning the outcome,
    /// the sudo passwords passed to the runtime installer, and the models passed
    /// to the model pull.
    fn run_with_install(
        keys: Vec<io::Result<Key>>,
        config: Config,
        install_result: fn(&str) -> Result<()>,
        pull_result: fn(&str) -> Result<()>,
    ) -> (Outcome, Vec<String>, Vec<String>) {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(keys);
        let mut save: fn(&Settings, Option<&LocalSettings>) -> Result<()> = noop_save;
        let passwords = RefCell::new(Vec::new());
        let pulled = RefCell::new(Vec::new());
        let mut install = |password: &str| {
            passwords.borrow_mut().push(password.to_string());
            install_result(password)
        };
        let mut pull = |model: &str| {
            pulled.borrow_mut().push(model.to_string());
            pull_result(model)
        };
        let outcome = event_loop(
            &term,
            &mut reader,
            config,
            &mut save,
            &mut install,
            &mut pull,
            None,
        )
        .unwrap();
        (outcome, passwords.into_inner(), pulled.into_inner())
    }

    fn failing_install(_: &str) -> Result<()> {
        Err(anyhow::anyhow!("sudo failed"))
    }

    fn failing_pull(_: &str) -> Result<()> {
        Err(anyhow::anyhow!("pull failed"))
    }

    /// Press ArrowDown to land on the Local LLM install row.
    fn keys_to_local_llm() -> Vec<io::Result<Key>> {
        vec![
            Ok(Key::ArrowDown), // Default Workspace
            Ok(Key::ArrowDown), // Notifications
            Ok(Key::ArrowDown), // Agent CLI
            Ok(Key::ArrowDown), // Session Action UI
            Ok(Key::ArrowDown), // Local LLM
        ]
    }

    /// A config with the runtime already installed, so the Local LLM row is an
    /// on/off toggle and the model row opens the picker.
    fn installed_config() -> Config {
        let mut config = sample_config();
        config.set_ollama_installed(true);
        config
    }

    #[test]
    fn space_opens_the_install_modal_and_confirms_with_the_typed_password() {
        let mut keys = keys_to_local_llm();
        keys.extend([
            Ok(Key::Char(' ')),  // open the install modal
            Ok(Key::Char('p')),  // type the sudo password
            Ok(Key::Char('X')),  // a stray character to edit out
            Ok(Key::Char('w')),  // "pXw"
            Ok(Key::Home),       // caret to the start
            Ok(Key::ArrowRight), // caret after 'p'
            Ok(Key::Del),        // forward-delete 'X' -> "pw"
            Ok(Key::End),        // caret to the end
            Ok(Key::ArrowLeft),  // caret before 'w'
            Ok(Key::ArrowRight), // caret after 'w' (end)
            Ok(Key::ArrowUp),    // ignored inside the modal
            Ok(Key::Enter),      // confirm -> install
            Ok(Key::Escape),
        ]);
        let (outcome, passwords, pulled) =
            run_with_install(keys, sample_config(), ok_install, ok_pull);
        assert!(matches!(outcome, Outcome::Back));
        // The runtime install runs with the edited password; no model is pulled.
        assert_eq!(passwords, vec!["pw".to_string()]);
        assert!(pulled.is_empty());
    }

    #[test]
    fn enter_on_the_install_action_also_opens_the_modal() {
        let mut keys = keys_to_local_llm();
        keys.extend([
            Ok(Key::Enter), // open the modal (install action focused)
            Ok(Key::Enter), // confirm with an empty password
            Ok(Key::Escape),
        ]);
        let (_, passwords, _) = run_with_install(keys, sample_config(), ok_install, ok_pull);
        assert_eq!(passwords, vec![String::new()]);
    }

    #[test]
    fn arrows_on_the_install_action_do_not_install() {
        let mut keys = keys_to_local_llm();
        keys.extend([Ok(Key::ArrowRight), Ok(Key::ArrowLeft), Ok(Key::Escape)]);
        let (outcome, passwords, _) = run_with_install(keys, sample_config(), ok_install, ok_pull);
        assert!(matches!(outcome, Outcome::Back));
        assert!(passwords.is_empty());
    }

    #[test]
    fn space_off_the_install_action_does_nothing() {
        // Space on a normal field (Theme) neither opens a modal nor installs.
        let keys = vec![Ok(Key::Char(' ')), Ok(Key::Escape)];
        let (outcome, passwords, _) = run_with_install(keys, sample_config(), ok_install, ok_pull);
        assert!(matches!(outcome, Outcome::Back));
        assert!(passwords.is_empty());
    }

    #[test]
    fn escape_cancels_the_install_modal_without_installing() {
        let mut keys = keys_to_local_llm();
        keys.extend([
            Ok(Key::Char(' ')), // open
            Ok(Key::Char('x')), // type a character
            Ok(Key::Backspace), // delete it
            Ok(Key::Escape),    // cancel the modal
            Ok(Key::Escape),    // leave the screen
        ]);
        let (outcome, passwords, _) = run_with_install(keys, sample_config(), ok_install, ok_pull);
        assert!(matches!(outcome, Outcome::Back));
        assert!(passwords.is_empty());
    }

    #[test]
    fn ctrl_c_in_the_install_modal_quits() {
        let mut keys = keys_to_local_llm();
        keys.extend([Ok(Key::Char(' ')), Ok(Key::CtrlC)]);
        let (outcome, passwords, _) = run_with_install(keys, sample_config(), ok_install, ok_pull);
        assert!(matches!(outcome, Outcome::Quit));
        assert!(passwords.is_empty());
    }

    #[test]
    fn a_failed_install_is_shown_as_a_notice_and_recovers() {
        let mut keys = keys_to_local_llm();
        keys.extend([
            Ok(Key::Char(' ')),
            Ok(Key::Enter), // confirm -> install fails
            Ok(Key::Escape),
        ]);
        let (outcome, passwords, _) =
            run_with_install(keys, sample_config(), failing_install, ok_pull);
        // The loop keeps running (the user can retry or leave).
        assert!(matches!(outcome, Outcome::Back));
        assert_eq!(passwords, vec![String::new()]);
    }

    // --- local LLM model picker -------------------------------------------

    /// Press ArrowDown to land on the Local LLM Model row.
    fn keys_to_model_row() -> Vec<io::Result<Key>> {
        let mut keys = keys_to_local_llm();
        keys.push(Ok(Key::ArrowDown)); // Local LLM Model
        keys
    }

    #[test]
    fn enter_on_the_model_row_opens_the_picker_and_pulls_an_uninstalled_choice() {
        let mut keys = keys_to_model_row();
        keys.extend([
            Ok(Key::Enter),     // open the picker (model row focused)
            Ok(Key::ArrowDown), // onto "qwen2.5-coder:3b" (not pulled)
            Ok(Key::Enter),     // confirm -> pull then adopt
            Ok(Key::Escape),
        ]);
        let (outcome, passwords, pulled) =
            run_with_install(keys, installed_config(), ok_install, ok_pull);
        assert!(matches!(outcome, Outcome::Back));
        // The model is pulled (no runtime install runs).
        assert!(passwords.is_empty());
        assert_eq!(pulled, vec!["qwen2.5-coder:3b".to_string()]);
    }

    #[test]
    fn selecting_an_already_installed_model_does_not_pull() {
        let mut config = installed_config();
        config.set_installed_models(vec!["qwen2.5-coder:3b".to_string()]);
        let mut keys = keys_to_model_row();
        keys.extend([
            Ok(Key::Char(' ')), // open the picker
            Ok(Key::Char('j')), // onto "qwen2.5-coder:3b" (already pulled)
            Ok(Key::Enter),     // adopt directly, no pull
            Ok(Key::Escape),
        ]);
        let (outcome, _, pulled) = run_with_install(keys, config, ok_install, ok_pull);
        assert!(matches!(outcome, Outcome::Back));
        assert!(pulled.is_empty());
    }

    #[test]
    fn escape_cancels_the_model_picker_without_pulling() {
        let mut keys = keys_to_model_row();
        keys.extend([
            Ok(Key::Enter),     // open the picker
            Ok(Key::ArrowDown), // move the cursor down
            Ok(Key::Char('k')), // back up via the vim alias
            Ok(Key::ArrowUp),   // and up again (wraps)
            Ok(Key::Home),      // an ignored key inside the picker
            Ok(Key::Escape),    // cancel the picker
            Ok(Key::Escape),    // leave the screen
        ]);
        let (outcome, _, pulled) = run_with_install(keys, installed_config(), ok_install, ok_pull);
        assert!(matches!(outcome, Outcome::Back));
        assert!(pulled.is_empty());
    }

    #[test]
    fn ctrl_c_in_the_model_picker_quits() {
        let mut keys = keys_to_model_row();
        keys.extend([Ok(Key::Enter), Ok(Key::CtrlC)]);
        let (outcome, _, pulled) = run_with_install(keys, installed_config(), ok_install, ok_pull);
        assert!(matches!(outcome, Outcome::Quit));
        assert!(pulled.is_empty());
    }

    #[test]
    fn a_failed_pull_is_shown_as_a_notice_and_recovers() {
        let mut keys = keys_to_model_row();
        keys.extend([
            Ok(Key::Enter),     // open the picker
            Ok(Key::ArrowDown), // onto an unpulled model
            Ok(Key::Enter),     // confirm -> pull fails
            Ok(Key::Escape),
        ]);
        let (outcome, _, pulled) =
            run_with_install(keys, installed_config(), ok_install, failing_pull);
        assert!(matches!(outcome, Outcome::Back));
        assert_eq!(pulled, vec!["qwen2.5-coder:3b".to_string()]);
    }

    #[test]
    fn the_model_row_is_inert_before_the_runtime_is_installed() {
        // Without the runtime installed the model row neither cycles nor opens a
        // picker, so Enter/Space/arrows pull nothing.
        let mut keys = keys_to_model_row();
        keys.extend([
            Ok(Key::Enter),
            Ok(Key::Char(' ')),
            Ok(Key::ArrowRight),
            Ok(Key::Escape),
        ]);
        let (outcome, _, pulled) = run_with_install(keys, sample_config(), ok_install, ok_pull);
        assert!(matches!(outcome, Outcome::Back));
        assert!(pulled.is_empty());
    }

    #[test]
    fn toggling_the_local_llm_after_install_persists_the_enabled_flag() {
        // Pretend the runtime is already installed, so the row is an on/off
        // toggle. Down onto it, → toggles On, then save persists enabled = true.
        let config = installed_config();
        let keys = vec![
            Ok(Key::ArrowDown),  // Default Workspace
            Ok(Key::ArrowDown),  // Notifications
            Ok(Key::ArrowDown),  // Agent CLI
            Ok(Key::ArrowDown),  // Session Action UI
            Ok(Key::ArrowDown),  // Local LLM
            Ok(Key::ArrowRight), // toggle On
            Ok(Key::ArrowDown),  // Local LLM Model
            Ok(Key::ArrowDown),  // Save button
            Ok(Key::Enter),      // save
            Ok(Key::Escape),
        ];
        let (_, saved) = run_recording(keys, config);
        assert_eq!(saved.len(), 1);
        assert!(saved[0].local_llm.enabled);
    }
}

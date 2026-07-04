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

/// A config with the `ollama` runtime already installed, so the Local LLM
/// row is an on/off toggle and the model row opens the picker.
fn installed_config() -> Config {
    let mut config = sample_config();
    config.set_ollama_installed(true);
    config
}

/// A persistence stub that accepts every change (and is itself exercised by
/// [`saving_succeeds_with_a_noop_save`]).
fn noop_save(_: &Settings, _: Option<&LocalSettings>) -> Result<()> {
    Ok(())
}

/// A runtime-install stub that succeeds without doing anything.
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
fn ctrl_q_returns_quit() {
    // `Ctrl-Q` (the bare `0x11`) quits from the config screen too.
    let (outcome, _) = run_recording(vec![Ok(Key::Char('\u{0011}'))], sample_config());
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
    let keys = vec![
        Ok(Key::ArrowDown),  // Default Workspace
        Ok(Key::ArrowRight), // -> alpha
        Ok(Key::ArrowDown),  // Notifications
        Ok(Key::ArrowDown),  // Restore Panes
        Ok(Key::ArrowDown),  // Agent CLI
        Ok(Key::ArrowDown),  // Session Action UI
        Ok(Key::ArrowDown),  // Terminal Keys
        Ok(Key::ArrowDown),  // Mascot Animation
        Ok(Key::ArrowDown),  // Local LLM
        Ok(Key::ArrowDown),  // Local LLM Model
        Ok(Key::ArrowDown),  // Env Vars
        Ok(Key::ArrowDown),  // PR Skills
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
    let term = Term::stdout();
    let config = Config::workspace(Settings::default(), LocalSettings::default(), Vec::new());
    let keys = vec![
        Ok(Key::ArrowRight), // Agent CLI override: Global -> Claude
        Ok(Key::ArrowDown),  // Notifications
        Ok(Key::ArrowDown),  // Restore Panes
        Ok(Key::ArrowDown),  // Default Branch
        Ok(Key::ArrowDown),  // Branch Source
        Ok(Key::ArrowDown),  // Setup Commands
        Ok(Key::ArrowDown),  // Env Vars
        Ok(Key::ArrowDown),  // Session Labels
        Ok(Key::ArrowDown),  // PR Skills
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
fn setup_commands_modal_applies_to_local_settings_before_save() {
    let term = Term::stdout();
    let config = Config::workspace(Settings::default(), LocalSettings::default(), Vec::new());
    let keys = vec![
        Ok(Key::ArrowDown), // Notifications
        Ok(Key::ArrowDown), // Restore Panes
        Ok(Key::ArrowDown), // Default Branch
        Ok(Key::ArrowDown), // Branch Source
        Ok(Key::ArrowDown), // Setup Commands
        Ok(Key::Enter),     // open editor
        Ok(Key::Char('a')),
        Ok(Key::Char('b')),
        Ok(Key::ArrowLeft),
        Ok(Key::Char('X')),
        Ok(Key::ArrowRight),
        Ok(Key::Backspace),
        Ok(Key::Home),
        Ok(Key::Del),
        Ok(Key::End),
        Ok(Key::Enter), // second command line
        Ok(Key::Char(' ')),
        Ok(Key::Char('c')),
        Ok(Key::Char(' ')),
        Ok(Key::ArrowUp),
        Ok(Key::ArrowDown),
        Ok(Key::Char('\u{0013}')), // Ctrl-S applies the modal buffer
        Ok(Key::ArrowDown),        // Env Vars
        Ok(Key::ArrowDown),        // Session Labels
        Ok(Key::ArrowDown),        // PR Skills
        Ok(Key::ArrowDown),        // Save
        Ok(Key::Enter),
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
    assert_eq!(local.setup_commands, vec!["X".to_string(), "c".to_string()]);
}

#[test]
fn setup_commands_modal_can_be_cancelled_and_can_quit() {
    let term = Term::stdout();
    let to_setup = || {
        vec![
            Ok(Key::ArrowDown),
            Ok(Key::ArrowDown),
            Ok(Key::ArrowDown),
            Ok(Key::ArrowDown),
            Ok(Key::ArrowDown),
        ]
    };

    let mut keys = to_setup();
    keys.extend([
        Ok(Key::Char(' ')), // open editor with Space
        Ok(Key::Char('x')),
        Ok(Key::Escape), // cancel editor
        Ok(Key::Escape), // leave config
    ]);
    let mut reader = ScriptedReader::new(keys);
    let captured: RefCell<Option<LocalSettings>> = RefCell::new(None);
    let mut save = |_: &Settings, local: Option<&LocalSettings>| {
        *captured.borrow_mut() = local.cloned();
        Ok(())
    };
    let mut install: fn(&str) -> Result<()> = ok_install;
    let mut pull: fn(&str) -> Result<()> = ok_pull;
    assert!(matches!(
        event_loop(
            &term,
            &mut reader,
            Config::workspace(Settings::default(), LocalSettings::default(), Vec::new()),
            &mut save,
            &mut install,
            &mut pull,
            None,
        )
        .unwrap(),
        Outcome::Back
    ));
    assert!(captured.into_inner().is_none());

    let mut keys = to_setup();
    keys.extend([Ok(Key::Enter), Ok(Key::CtrlC)]);
    let mut reader = ScriptedReader::new(keys);
    let mut save: fn(&Settings, Option<&LocalSettings>) -> Result<()> = noop_save;
    assert!(matches!(
        event_loop(
            &term,
            &mut reader,
            Config::workspace(Settings::default(), LocalSettings::default(), Vec::new()),
            &mut save,
            &mut install,
            &mut pull,
            None,
        )
        .unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn env_vars_modal_applies_to_local_settings_before_save() {
    let term = Term::stdout();
    let config = Config::workspace(Settings::default(), LocalSettings::default(), Vec::new());
    let keys = vec![
        Ok(Key::ArrowDown), // Notifications
        Ok(Key::ArrowDown), // Restore Panes
        Ok(Key::ArrowDown), // Default Branch
        Ok(Key::ArrowDown), // Branch Source
        Ok(Key::ArrowDown), // Setup Commands
        Ok(Key::ArrowDown), // Env Vars
        Ok(Key::Enter),     // open editor
        Ok(Key::Tab),       // an unhandled key inside the editor is ignored
        Ok(Key::Char('a')),
        Ok(Key::Char('b')),
        Ok(Key::ArrowLeft),
        Ok(Key::Char('X')),
        Ok(Key::ArrowRight),
        Ok(Key::Backspace),
        Ok(Key::Home),
        Ok(Key::Del),
        Ok(Key::End),
        Ok(Key::Enter), // second line
        Ok(Key::Char('G')),
        Ok(Key::Char('=')),
        Ok(Key::Char('o')),
        Ok(Key::Char('p')),
        Ok(Key::Char(':')),
        Ok(Key::Char('/')),
        Ok(Key::Char('/')),
        Ok(Key::Char('v')),
        Ok(Key::Char('/')),
        Ok(Key::Char('i')),
        Ok(Key::Char('/')),
        Ok(Key::Char('f')),
        Ok(Key::ArrowUp),
        Ok(Key::ArrowDown),
        Ok(Key::Char('\u{0013}')), // Ctrl-S applies the modal buffer
        Ok(Key::ArrowDown),        // Session Labels
        Ok(Key::ArrowDown),        // PR Skills
        Ok(Key::ArrowDown),        // Save
        Ok(Key::Enter),
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
    // The first line became "X" (an invalid name, dropped); the second is the
    // one valid binding.
    assert_eq!(local.env.get("G").map(String::as_str), Some("op://v/i/f"));
    assert_eq!(local.env.len(), 1);
}

#[test]
fn env_vars_modal_can_be_cancelled_and_can_quit() {
    let term = Term::stdout();
    let to_env = || {
        vec![
            Ok(Key::ArrowDown), // Notifications
            Ok(Key::ArrowDown), // Restore Panes
            Ok(Key::ArrowDown), // Default Branch
            Ok(Key::ArrowDown), // Branch Source
            Ok(Key::ArrowDown), // Setup Commands
            Ok(Key::ArrowDown), // Env Vars
        ]
    };

    let mut keys = to_env();
    keys.extend([
        Ok(Key::Char(' ')), // open editor with Space
        Ok(Key::Char('x')),
        Ok(Key::Escape), // cancel editor
        Ok(Key::Escape), // leave config
    ]);
    let mut reader = ScriptedReader::new(keys);
    let captured: RefCell<Option<LocalSettings>> = RefCell::new(None);
    let mut save = |_: &Settings, local: Option<&LocalSettings>| {
        *captured.borrow_mut() = local.cloned();
        Ok(())
    };
    let mut install: fn(&str) -> Result<()> = ok_install;
    let mut pull: fn(&str) -> Result<()> = ok_pull;
    assert!(matches!(
        event_loop(
            &term,
            &mut reader,
            Config::workspace(Settings::default(), LocalSettings::default(), Vec::new()),
            &mut save,
            &mut install,
            &mut pull,
            None,
        )
        .unwrap(),
        Outcome::Back
    ));
    assert!(captured.into_inner().is_none());

    let mut keys = to_env();
    keys.extend([Ok(Key::Enter), Ok(Key::CtrlC)]);
    let mut reader = ScriptedReader::new(keys);
    let mut save: fn(&Settings, Option<&LocalSettings>) -> Result<()> = noop_save;
    assert!(matches!(
        event_loop(
            &term,
            &mut reader,
            Config::workspace(Settings::default(), LocalSettings::default(), Vec::new()),
            &mut save,
            &mut install,
            &mut pull,
            None,
        )
        .unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn session_labels_modal_applies_to_local_settings_before_save() {
    let term = Term::stdout();
    let config = Config::workspace(Settings::default(), LocalSettings::default(), Vec::new());
    let keys = vec![
        Ok(Key::ArrowDown), // Notifications
        Ok(Key::ArrowDown), // Restore Panes
        Ok(Key::ArrowDown), // Default Branch
        Ok(Key::ArrowDown), // Branch Source
        Ok(Key::ArrowDown), // Setup Commands
        Ok(Key::ArrowDown), // Env Vars
        Ok(Key::ArrowDown), // Session Labels
        Ok(Key::Enter),     // open editor (seeded with the 5 default labels)
        Ok(Key::Tab),       // an unhandled key inside the editor is ignored
        Ok(Key::ArrowDown), // walk down to the last seeded line
        Ok(Key::ArrowDown),
        Ok(Key::ArrowDown),
        Ok(Key::ArrowDown),
        Ok(Key::End),
        Ok(Key::Enter), // append a new label line
        Ok(Key::Char('u')),
        Ok(Key::Char('r')),
        Ok(Key::Char('g')),
        Ok(Key::Char('e')),
        Ok(Key::Char('n')),
        Ok(Key::Char('t')),
        Ok(Key::Char(' ')),
        Ok(Key::Char('|')),
        Ok(Key::Char(' ')),
        Ok(Key::Char('U')),
        Ok(Key::Char('r')),
        Ok(Key::Char('g')),
        Ok(Key::Char(' ')),
        Ok(Key::Char('|')),
        Ok(Key::Char(' ')),
        Ok(Key::Char('r')),
        Ok(Key::Char('e')),
        Ok(Key::Char('d')),
        Ok(Key::Char('Z')),
        Ok(Key::Backspace), // drop the trailing Z
        Ok(Key::ArrowLeft),
        Ok(Key::ArrowRight),
        Ok(Key::Home),
        Ok(Key::Del), // drop the leading char of the new line's id
        Ok(Key::ArrowUp),
        Ok(Key::Char('\u{0013}')), // Ctrl-S applies the modal buffer
        Ok(Key::ArrowDown),        // PR Skills
        Ok(Key::ArrowDown),        // Save
        Ok(Key::Enter),
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
    let master = local.session_labels.expect("override stored");
    // The five defaults plus the appended label (its "Urg" name intact after the
    // Backspace/Del edits).
    assert_eq!(master.labels().len(), 6);
    assert!(master.labels().iter().any(|l| l.name == "Urg"));
}

#[test]
fn session_labels_modal_can_be_cancelled_and_can_quit() {
    let term = Term::stdout();
    let to_labels = || {
        vec![
            Ok(Key::ArrowDown), // Notifications
            Ok(Key::ArrowDown), // Restore Panes
            Ok(Key::ArrowDown), // Default Branch
            Ok(Key::ArrowDown), // Branch Source
            Ok(Key::ArrowDown), // Setup Commands
            Ok(Key::ArrowDown), // Env Vars
            Ok(Key::ArrowDown), // Session Labels
        ]
    };

    let mut keys = to_labels();
    keys.extend([
        Ok(Key::Char(' ')), // open editor with Space
        Ok(Key::Char('x')),
        Ok(Key::Escape), // cancel editor
        Ok(Key::Escape), // leave config
    ]);
    let mut reader = ScriptedReader::new(keys);
    let captured: RefCell<Option<LocalSettings>> = RefCell::new(None);
    let mut save = |_: &Settings, local: Option<&LocalSettings>| {
        *captured.borrow_mut() = local.cloned();
        Ok(())
    };
    let mut install: fn(&str) -> Result<()> = ok_install;
    let mut pull: fn(&str) -> Result<()> = ok_pull;
    assert!(matches!(
        event_loop(
            &term,
            &mut reader,
            Config::workspace(Settings::default(), LocalSettings::default(), Vec::new()),
            &mut save,
            &mut install,
            &mut pull,
            None,
        )
        .unwrap(),
        Outcome::Back
    ));
    // Cancelling leaves no override, so nothing was dirty to save.
    assert!(captured.into_inner().is_none());

    let mut keys = to_labels();
    keys.extend([Ok(Key::Enter), Ok(Key::CtrlC)]);
    let mut reader = ScriptedReader::new(keys);
    let mut save: fn(&Settings, Option<&LocalSettings>) -> Result<()> = noop_save;
    assert!(matches!(
        event_loop(
            &term,
            &mut reader,
            Config::workspace(Settings::default(), LocalSettings::default(), Vec::new()),
            &mut save,
            &mut install,
            &mut pull,
            None,
        )
        .unwrap(),
        Outcome::Quit
    ));
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

// --- local LLM runtime install + model picker -------------------------

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
        Ok(Key::ArrowDown), // Restore Panes
        Ok(Key::ArrowDown), // Agent CLI
        Ok(Key::ArrowDown), // Session Action UI
        Ok(Key::ArrowDown), // Terminal Keys
        Ok(Key::ArrowDown), // Mascot Animation
        Ok(Key::ArrowDown), // Local LLM
    ]
}

/// Press ArrowDown to land on the Local LLM Model row.
fn keys_to_model_row() -> Vec<io::Result<Key>> {
    let mut keys = keys_to_local_llm();
    keys.push(Ok(Key::ArrowDown)); // Local LLM Model
    keys
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
        Ok(Key::Enter),      // confirm -> start runtime install
        Ok(Key::Escape),
    ]);
    let (outcome, passwords, pulled) = run_with_install(keys, sample_config(), ok_install, ok_pull);
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
fn ctrl_q_in_the_install_modal_quits() {
    // `Ctrl-Q` arrives as `Key::Char('\u{0011}')`; it must quit even mid-entry
    // rather than being captured as a password character by the `Char(c)` arm.
    let mut keys = keys_to_local_llm();
    keys.extend([Ok(Key::Char(' ')), Ok(Key::Char('\u{0011}'))]);
    let (outcome, passwords, _) = run_with_install(keys, sample_config(), ok_install, ok_pull);
    assert!(matches!(outcome, Outcome::Quit));
    assert!(passwords.is_empty());
}

#[test]
fn a_failed_install_is_shown_as_a_notice_and_recovers() {
    let mut keys = keys_to_local_llm();
    keys.extend([
        Ok(Key::Char(' ')),
        Ok(Key::Enter), // confirm -> install start fails
        Ok(Key::Escape),
    ]);
    let (outcome, passwords, _) = run_with_install(keys, sample_config(), failing_install, ok_pull);
    assert!(matches!(outcome, Outcome::Back));
    assert_eq!(passwords, vec![String::new()]);
}

#[test]
fn enter_on_the_model_row_opens_the_picker_and_pulls_an_uninstalled_choice() {
    let mut keys = keys_to_model_row();
    keys.extend([
        Ok(Key::Enter),     // open the picker (model row focused)
        Ok(Key::ArrowDown), // onto "qwen2.5-coder:3b" (not pulled)
        Ok(Key::Enter),     // confirm -> start background pull
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
fn ctrl_q_in_the_model_picker_quits() {
    // `Ctrl-Q` (the bare `0x11`) quits from the model picker too.
    let mut keys = keys_to_model_row();
    keys.extend([Ok(Key::Enter), Ok(Key::Char('\u{0011}'))]);
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
        Ok(Key::Enter),     // confirm -> pull start fails
        Ok(Key::Escape),
    ]);
    let (outcome, _, pulled) = run_with_install(keys, installed_config(), ok_install, failing_pull);
    assert!(matches!(outcome, Outcome::Back));
    assert_eq!(pulled, vec!["qwen2.5-coder:3b".to_string()]);
}

#[test]
fn the_model_row_is_inert_before_the_runtime_is_installed() {
    // Without the runtime installed the model row neither cycles nor opens a
    // picker, so Enter/Space pull nothing.
    let mut keys = keys_to_model_row();
    keys.extend([Ok(Key::Enter), Ok(Key::Char(' ')), Ok(Key::Escape)]);
    let (outcome, _, pulled) = run_with_install(keys, sample_config(), ok_install, ok_pull);
    assert!(matches!(outcome, Outcome::Back));
    assert!(pulled.is_empty());
}

#[test]
fn arrows_on_the_active_model_row_are_a_noop() {
    // With the runtime installed the model row opens a picker, so ←/→ have
    // nothing to cycle and surface no hint.
    let mut keys = keys_to_model_row();
    keys.extend([Ok(Key::ArrowRight), Ok(Key::ArrowLeft), Ok(Key::Escape)]);
    let (outcome, _, pulled) = run_with_install(keys, installed_config(), ok_install, ok_pull);
    assert!(matches!(outcome, Outcome::Back));
    assert!(pulled.is_empty());
}

#[test]
fn run_model_select_is_a_noop_without_an_open_picker() {
    // Defensive: with no picker open there is no selection, so it does
    // nothing and pulls nothing.
    let mut config = installed_config();
    let mut pull: fn(&str) -> Result<()> = ok_pull;
    assert_eq!(run_model_select(&mut config, &mut pull), None);
}

// --- reflecting a finished background install -------------------------

#[test]
fn reflect_install_flips_the_runtime_row_and_focuses_the_model_row() {
    // A finished runtime install flips the Local LLM row to installed and
    // drops the cursor onto the model row.
    let mut config = sample_config();
    config.set_pending_install(PendingInstall::Runtime);
    assert!(!config.ollama_installed());
    let view = InstallView::Done {
        ok: true,
        message: "ollama を導入しました 󰤇".to_string(),
    };
    let note = reflect_install(&mut config, Some(&view));
    assert_eq!(note.as_deref(), Some("ollama を導入しました 󰤇"));
    assert!(config.ollama_installed());
    assert_eq!(
        config.selected_field(),
        Some(super::super::state::Field::LocalLlmModel)
    );
}

#[test]
fn reflect_install_records_a_pulled_model() {
    // A finished model pull records the model installed and adopts it.
    let mut config = installed_config();
    config.set_pending_install(PendingInstall::Model("qwen2.5-coder:3b".to_string()));
    let view = InstallView::Done {
        ok: true,
        message: "qwen2.5-coder:3b を導入しました 󰤇".to_string(),
    };
    let note = reflect_install(&mut config, Some(&view));
    assert_eq!(note.as_deref(), Some("qwen2.5-coder:3b を導入しました 󰤇"));
    assert_eq!(config.local_llm_model(), "qwen2.5-coder:3b");
}

#[test]
fn reflect_install_is_idempotent_and_ignores_non_success() {
    // With nothing pending a success view has nothing to apply.
    let mut config = sample_config();
    let done = InstallView::Done {
        ok: true,
        message: "x".to_string(),
    };
    assert_eq!(reflect_install(&mut config, Some(&done)), None);

    // Running, failed, and absent views never apply (a failure is shown by
    // the overlay instead), even with a pending install.
    let mut fresh = sample_config();
    fresh.set_pending_install(PendingInstall::Runtime);
    let running = InstallView::Running {
        label: "l".to_string(),
        hop_frame: 0,
        face_index: 0,
    };
    assert_eq!(reflect_install(&mut fresh, Some(&running)), None);
    let failed = InstallView::Done {
        ok: false,
        message: "x".to_string(),
    };
    assert_eq!(reflect_install(&mut fresh, Some(&failed)), None);
    assert_eq!(reflect_install(&mut fresh, None), None);
    assert!(!fresh.ollama_installed());
}

#[test]
fn reflect_install_skips_the_workspace_scope() {
    // A workspace-scoped config has no Local LLM row, so a completed install
    // never applies there.
    let mut workspace =
        Config::workspace(Settings::default(), LocalSettings::default(), Vec::new());
    let done = InstallView::Done {
        ok: true,
        message: "x".to_string(),
    };
    assert_eq!(reflect_install(&mut workspace, Some(&done)), None);
}

#[test]
fn toggling_the_local_llm_after_install_persists_the_enabled_flag() {
    // The runtime is already installed, so the row is an on/off toggle.
    // Down onto it, → toggles On, then save persists enabled = true.
    let config = installed_config();
    let keys = vec![
        Ok(Key::ArrowDown),  // Default Workspace
        Ok(Key::ArrowDown),  // Notifications
        Ok(Key::ArrowDown),  // Restore Panes
        Ok(Key::ArrowDown),  // Agent CLI
        Ok(Key::ArrowDown),  // Session Action UI
        Ok(Key::ArrowDown),  // Terminal Keys
        Ok(Key::ArrowDown),  // Mascot Animation
        Ok(Key::ArrowDown),  // Local LLM
        Ok(Key::ArrowRight), // toggle On
        Ok(Key::ArrowDown),  // Local LLM Model
        Ok(Key::ArrowDown),  // Env Vars
        Ok(Key::ArrowDown),  // PR Skills
        Ok(Key::ArrowDown),  // Save button
        Ok(Key::Enter),      // save
        Ok(Key::Escape),
    ];
    let (_, saved) = run_recording(keys, config);
    assert_eq!(saved.len(), 1);
    assert!(saved[0].local_llm.enabled);
}

#[test]
fn unhandled_keys_inside_the_setup_modal_are_silently_ignored() {
    // Press PageUp while the setup-commands editor is open — it matches the
    // `_ => {}` catch-all and must not close or corrupt the modal.
    let term = Term::stdout();
    let config = Config::workspace(Settings::default(), LocalSettings::default(), Vec::new());
    let keys = vec![
        Ok(Key::ArrowDown), // Notifications
        Ok(Key::ArrowDown), // Restore Panes
        Ok(Key::ArrowDown), // Default Branch
        Ok(Key::ArrowDown), // Branch Source
        Ok(Key::ArrowDown), // Setup Commands
        Ok(Key::Enter),     // open modal
        Ok(Key::PageUp),    // unhandled key — exercises the `_ => {}` branch
        Ok(Key::Escape),    // close modal
        Ok(Key::Escape),    // leave config
    ];
    let mut reader = ScriptedReader::new(keys);
    let mut save: fn(&Settings, Option<&LocalSettings>) -> Result<()> = noop_save;
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
}

use anyhow::Result;
use console::Key;
use console::Term;

use crate::domain::settings::{LocalSettings, Settings};
use crate::presentation::tui::install_task::{self, InstallView};
use crate::presentation::tui::io::screen::{animated_read, FramePainter, KeyReader};

use super::state::{Config, PendingInstall};
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

/// Starts provisioning the `ollama` runtime in the background, taking the sudo
/// password entered in the install modal so the install can elevate
/// non-interactively. Returns as soon as the work is *launched* — the install
/// then runs on its own thread while the user keeps using usagi, its progress
/// surfaced everywhere by the global install overlay. Injected like [`Save`] so
/// the event loop is testable without shelling out: production wires it to
/// [`install_task`], tests pass a stub.
pub type InstallRuntime<'a> = dyn FnMut(&str) -> Result<()> + 'a;

/// Starts pulling a model into the installed runtime in the background (the
/// model picker's "install on select" path). Like [`InstallRuntime`] it returns
/// as soon as the pull is launched; `ollama pull` is unprivileged, so it takes
/// only the model name.
pub type PullModel<'a> = dyn FnMut(&str) -> Result<()> + 'a;

/// Runs the configuration screen against the given terminal and key source
/// until the user goes back or quits. Assumes the alternate screen is already
/// active (it is owned by the caller).
///
/// Changing a setting (←/→, or Enter on a field) edits it in memory only — the
/// row is flagged as changed but nothing touches disk. The edits are written
/// only when the user moves to the Save button and presses Enter; a persistence
/// failure is shown as a notice so the user is not left wondering whether the
/// change took. The Local LLM row is the exception: while the runtime/model is
/// missing it is an "Install" action — Space or Enter opens a modal that
/// collects the sudo password, and confirming runs `install` (provisioning is
/// an action, not a saved setting). The cursor then drops onto the model row.
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
        // When the background install finishes, flip the Local LLM row to its
        // installed state and surface the outcome — picked up on the next loop
        // pass (every key press), while the overlay shows it live in the corner.
        // A completion message takes precedence over any standing notice.
        notice = reflect_install(&mut config, install_task::snapshot().as_ref()).or(notice);

        let (height, width) = term.size();
        let frame = ui::render_frame(height as usize, width as usize, &config, notice.as_deref());
        painter.paint(term, frame)?;

        // While an install runs the read wakes periodically to animate the
        // overlay; otherwise it blocks as usual.
        let key = match animated_read(reader, term, &mut painter, &install_task::handle()) {
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
                // `Ctrl-Q` (the bare `0x11`) quits even mid-entry, matched before the
                // `Key::Char(c)` arm below that would otherwise capture it as input.
                Key::Char('\u{0011}') => return Ok(Outcome::Quit),
                Key::Enter => {
                    notice = run_install(&mut config, install_runtime);
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
        // Enter adopts the highlighted model (starting a background pull first
        // when it is not yet present), and Esc cancels.
        if config.model_modal().is_some() {
            match key {
                Key::ArrowUp | Key::Char('k') => config.model_modal_up(),
                Key::ArrowDown | Key::Char('j') => config.model_modal_down(),
                Key::Enter => {
                    notice = run_model_select(&mut config, pull_model);
                }
                Key::Escape => config.close_model_modal(),
                Key::CtrlC | Key::Char('\u{0011}') => return Ok(Outcome::Quit),
                _ => {}
            }
            continue;
        }

        // The setup-command editor captures every key. Enter inserts a new
        // command line, Ctrl-S applies the buffer into the in-memory local
        // settings (still requiring the main Save button to persist), and Esc
        // cancels.
        if config.setup_modal().is_some() {
            match key {
                Key::Char('\u{0013}') => {
                    config.apply_setup_modal();
                    notice = None;
                }
                Key::Enter => config.setup_modal_newline(),
                Key::Backspace => config.setup_modal_backspace(),
                Key::Del => config.setup_modal_delete_forward(),
                Key::ArrowLeft => config.setup_modal_cursor_left(),
                Key::ArrowRight => config.setup_modal_cursor_right(),
                Key::ArrowUp => config.setup_modal_cursor_up(),
                Key::ArrowDown => config.setup_modal_cursor_down(),
                Key::Home => config.setup_modal_cursor_home(),
                Key::End => config.setup_modal_cursor_end(),
                Key::Char(c) => config.setup_modal_insert(c),
                Key::Escape => config.close_setup_modal(),
                Key::CtrlC => return Ok(Outcome::Quit),
                _ => {}
            }
            continue;
        }

        // The workspace-env editor captures every key, the same way the
        // setup-command editor does: Enter inserts a new binding line, Ctrl-S
        // applies the buffer into the in-memory local settings (still requiring
        // the main Save button to persist), and Esc cancels.
        if config.env_modal().is_some() {
            match key {
                Key::Char('\u{0013}') => {
                    config.apply_env_modal();
                    notice = None;
                }
                Key::Enter => config.env_modal_newline(),
                Key::Backspace => config.env_modal_backspace(),
                Key::Del => config.env_modal_delete_forward(),
                Key::ArrowLeft => config.env_modal_cursor_left(),
                Key::ArrowRight => config.env_modal_cursor_right(),
                Key::ArrowUp => config.env_modal_cursor_up(),
                Key::ArrowDown => config.env_modal_cursor_down(),
                Key::Home => config.env_modal_cursor_home(),
                Key::End => config.env_modal_cursor_end(),
                Key::Char(c) => config.env_modal_insert(c),
                Key::Escape => config.close_env_modal(),
                Key::CtrlC => return Ok(Outcome::Quit),
                _ => {}
            }
            continue;
        }

        // The session-label editor captures every key, the same way the setup /
        // env editors do: Enter inserts a new label line, Ctrl-S applies the
        // buffer into the in-memory local override (still requiring the main Save
        // button to persist), and Esc cancels.
        if config.session_labels_modal().is_some() {
            match key {
                Key::Char('\u{0013}') => {
                    config.apply_session_labels_modal();
                    notice = None;
                }
                Key::Enter => config.session_labels_modal_newline(),
                Key::Backspace => config.session_labels_modal_backspace(),
                Key::Del => config.session_labels_modal_delete_forward(),
                Key::ArrowLeft => config.session_labels_modal_cursor_left(),
                Key::ArrowRight => config.session_labels_modal_cursor_right(),
                Key::ArrowUp => config.session_labels_modal_cursor_up(),
                Key::ArrowDown => config.session_labels_modal_cursor_down(),
                Key::Home => config.session_labels_modal_cursor_home(),
                Key::End => config.session_labels_modal_cursor_end(),
                Key::Char(c) => config.session_labels_modal_insert(c),
                Key::Escape => config.close_session_labels_modal(),
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
                // the model picker on the active model row, or the setup-command /
                // env editor on their action rows; each is a no-op off its own row,
                // so calling all of them is safe.
                config.open_install_modal();
                config.open_model_modal();
                config.open_setup_modal();
                config.open_env_modal();
                config.open_session_labels_modal();
                notice = None;
            }
            Key::Enter => {
                // Enter saves on the Save button, opens the install modal on the
                // Local LLM install action, opens the model picker / setup editor
                // on their action rows, and otherwise advances the focused field
                // (a convenient alias for →).
                if config.is_save_selected() {
                    notice = save_changes(&mut config, save);
                } else if config.local_llm_needs_install() {
                    config.open_install_modal();
                    notice = None;
                } else if config.model_row_active() {
                    config.open_model_modal();
                    notice = None;
                } else if config.setup_row_active() {
                    config.open_setup_modal();
                    notice = None;
                } else if config.env_row_active() {
                    config.open_env_modal();
                    notice = None;
                } else if config.session_labels_row_active() {
                    config.open_session_labels_modal();
                    notice = None;
                } else {
                    notice = activate_field(&mut config, true);
                }
            }
            Key::Char('q') | Key::Escape => return Ok(Outcome::Back),
            // `Ctrl-C` / `Ctrl-Q` (the bare `0x11`) quit the app from config too.
            Key::CtrlC | Key::Char('\u{0011}') => return Ok(Outcome::Quit),
            _ => {}
        }
    }
}

/// Handles an arrow press on the focused field. The Local LLM install action
/// and the active model row have no value to cycle (both are driven by their
/// modals), so arrows are a no-op there; otherwise the field's value is cycled.
fn activate_field(config: &mut Config, forward: bool) -> Option<String> {
    if config.local_llm_needs_install()
        || config.model_row_active()
        || config.env_row_active()
        || config.session_labels_row_active()
    {
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
        Some("Nothing to choose from 󰤇".to_string())
    }
}

/// Starts the background runtime install with the sudo password from the modal,
/// closes the modal, and returns the notice to show. The install runs off-thread,
/// so this only reports that it *began* and records [`PendingInstall::Runtime`];
/// the Local LLM row flips to its installed state later, when the install
/// completes (see [`reflect_install`]).
fn run_install(config: &mut Config, install_runtime: &mut InstallRuntime) -> Option<String> {
    let password = config.install_modal_password().unwrap_or_default();
    let result = install_runtime(&password);
    config.close_install_modal();
    Some(match result {
        Ok(()) => {
            config.set_pending_install(PendingInstall::Runtime);
            "ランタイムのインストールを開始しました 󰤇".to_string()
        }
        Err(e) => format!("Install failed: {e}"),
    })
}

/// Adopts the model highlighted in the picker and closes the modal. An
/// already-installed model is adopted directly; an uninstalled one starts a
/// background pull and records [`PendingInstall::Model`] so its completion is
/// reflected (and the model adopted) when the pull finishes.
fn run_model_select(config: &mut Config, pull_model: &mut PullModel) -> Option<String> {
    let model = config.model_modal_selection()?.to_string();
    if config.model_modal_selection_installed() {
        config.select_model(&model);
        config.close_model_modal();
        return Some(format!("Using {model} 󰤇"));
    }
    let result = pull_model(&model);
    config.close_model_modal();
    Some(match result {
        Ok(()) => {
            config.set_pending_install(PendingInstall::Model(model.clone()));
            format!("{model} のインストールを開始しました 󰤇")
        }
        Err(e) => format!("Install failed: {e}"),
    })
}

/// Reflects a finished background install into the screen: when the install has
/// completed successfully, flip the Local LLM row to its installed toggle and
/// surface the completion message. Only the global scope carries that row (a
/// workspace's local overrides do not), and the install may finish while the
/// cursor is anywhere — so this guards on the scope and the not-yet-installed
/// flag rather than on the focused row, and leaves the cursor where it is. A
/// still-running install, a failure (whose message the overlay shows), or an
/// already-reflected success returns `None`, so it is idempotent across the loop
/// passes that call it.
fn reflect_install(config: &mut Config, view: Option<&InstallView>) -> Option<String> {
    if let Some(InstallView::Done { ok: true, message }) = view {
        if config.local().is_none() {
            if let Some(pending) = config.take_pending_install() {
                match pending {
                    // The runtime is now present: flip the Local LLM row to its
                    // on/off toggle and drop the cursor onto the model row.
                    PendingInstall::Runtime => {
                        config.mark_ollama_installed();
                        config.focus_model_row();
                    }
                    // The model was pulled: record it installed and adopt it.
                    PendingInstall::Model(model) => config.mark_model_installed(&model),
                }
                return Some(message.clone());
            }
        }
    }
    None
}

/// Persists the edits when there are any, returning the notice to show: a
/// confirmation, a save error, or a hint when there is nothing to save.
fn save_changes(config: &mut Config, save: &mut Save) -> Option<String> {
    if !config.is_dirty() {
        return Some("No changes to save 󰤇".to_string());
    }
    Some(match save(config.settings(), config.local()) {
        Ok(()) => {
            config.mark_saved();
            "Saved 󰤇".to_string()
        }
        Err(e) => format!("Failed to save: {e}"),
    })
}

#[cfg(test)]
mod tests;

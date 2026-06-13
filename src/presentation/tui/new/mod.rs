//! New Project screen (画面 #3).
//!
//! Collects a Git repository URL — and optionally a directory and branch —
//! the way editor "clone repository" dialogs do, then hands the validated
//! result back to the caller.

pub mod state;
pub mod ui;

use anyhow::Result;
use console::{Key, Term};

use state::{FormState, NewProject};

/// What the user chose to do on the New Project screen.
#[derive(Debug)]
pub enum Outcome {
    /// Return to the previous screen without creating a project.
    Back,
    /// The user submitted a valid project.
    Submitted(NewProject),
    /// The user asked to quit the application entirely.
    Quit,
}

/// Runs the New Project screen on the given terminal until the user submits,
/// goes back, or quits. Assumes the alternate screen is already active.
pub fn run(term: &Term) -> Result<Outcome> {
    let mut state = FormState::new();
    let mut notice: Option<String> = None;

    loop {
        term.move_cursor_to(0, 0)?;
        term.clear_screen()?;
        ui::render(term, &state, notice.as_deref());

        let key = match term.read_key() {
            Ok(key) => key,
            // An interrupted read (e.g. a delivered signal) means quit.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return Ok(Outcome::Quit),
            Err(e) => return Err(anyhow::Error::from(e).context("Failed to read key")),
        };

        match key {
            Key::Escape => return Ok(Outcome::Back),
            Key::CtrlC => return Ok(Outcome::Quit),
            Key::Enter => match state.validate() {
                Ok(project) => return Ok(Outcome::Submitted(project)),
                Err(message) => notice = Some(message),
            },
            Key::Tab | Key::ArrowDown => {
                state.focus_next();
                notice = None;
            }
            Key::BackTab | Key::ArrowUp => {
                state.focus_prev();
                notice = None;
            }
            Key::Backspace => {
                state.backspace();
                notice = None;
            }
            Key::Char(c) => {
                state.insert_char(c);
                notice = None;
            }
            _ => {}
        }
    }
}

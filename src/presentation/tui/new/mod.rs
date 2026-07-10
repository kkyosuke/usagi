//! New Project screen (画面 #3).
//!
//! Collects a Git repository URL — and optionally a directory and branch —
//! the way editor "clone repository" dialogs do, then hands the validated
//! result back to the caller.

pub mod event;
pub mod state;
pub mod ui;

use anyhow::Result;
use console::Term;

use crate::presentation::tui::io::term_reader::TermKeyReader;
use crate::presentation::tui::widgets::dir_picker::FsDirSource;
use state::FormState;

pub use event::Outcome;

#[cfg(not(test))]
use event::event_loop;
#[cfg(test)]
use tests::mock_event_loop as event_loop;

/// Runs the New Project screen on the given terminal until the user submits,
/// goes back, or quits. Wires the real terminal to the testable event loop in
/// [`event`]. Assumes the alternate screen is already active.
///
/// `default_location` pre-fills the Location field with the base directory new
/// projects are created under. `initial_form` and `notice` restore a submission
/// after project creation fails.
pub fn run(
    term: &Term,
    default_location: &str,
    initial_form: Option<FormState>,
    notice: Option<String>,
) -> Result<Outcome> {
    let mut reader = TermKeyReader::new(term.clone());
    let state = initial_form.unwrap_or_else(|| {
        let mut state = FormState::new();
        state.set_location(default_location);
        state
    });
    event_loop(
        term,
        &mut reader,
        state,
        notice,
        default_location,
        &FsDirSource,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::tui::io::screen::KeyReader;
    use crate::presentation::tui::widgets::dir_picker::DirSource;
    use anyhow::bail;
    use std::cell::RefCell;

    struct Mock {
        form: Option<FormState>,
        notice: Option<String>,
        result: Result<(), &'static str>,
    }

    thread_local! {
        /// The initial form / notice `run` forwarded, and the result the mock returns.
        static MOCK: RefCell<Mock> = const { RefCell::new(Mock {
            form: None,
            notice: None,
            result: Ok(()),
        }) };
    }

    /// Stands in for the real New Project loop so [`run`]'s wiring is exercised
    /// without blocking on real terminal input.
    pub(super) fn mock_event_loop(
        _term: &Term,
        _reader: &mut dyn KeyReader,
        state: FormState,
        notice: Option<String>,
        default_location: &str,
        _dir_source: &dyn DirSource,
    ) -> Result<Outcome> {
        MOCK.with(|m| {
            let mut m = m.borrow_mut();
            assert_eq!(default_location, "/tmp/projects");
            m.form = Some(state);
            m.notice = notice;
            match m.result {
                Ok(()) => Ok(Outcome::Back),
                Err(e) => bail!(e),
            }
        })
    }

    #[test]
    fn run_pre_fills_the_location_and_returns_the_outcome() {
        MOCK.with(|m| {
            *m.borrow_mut() = Mock {
                form: None,
                notice: None,
                result: Ok(()),
            }
        });
        let outcome = run(&Term::stdout(), "/tmp/projects", None, None).unwrap();
        assert!(matches!(outcome, Outcome::Back));
        // The default location is passed straight through to the form loop.
        MOCK.with(|m| {
            let m = m.borrow();
            assert_eq!(
                m.form.as_ref().map(FormState::location),
                Some("/tmp/projects")
            );
            assert_eq!(m.notice, None);
        });
    }

    #[test]
    fn run_forwards_a_preserved_form_and_notice() {
        MOCK.with(|m| {
            *m.borrow_mut() = Mock {
                form: None,
                notice: None,
                result: Ok(()),
            }
        });
        let mut form = FormState::new();
        form.set_location("/kept");
        let outcome = run(
            &Term::stdout(),
            "/tmp/projects",
            Some(form),
            Some("clone failed".to_string()),
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Back));
        MOCK.with(|m| {
            let m = m.borrow();
            assert_eq!(m.form.as_ref().map(FormState::location), Some("/kept"));
            assert_eq!(m.notice.as_deref(), Some("clone failed"));
        });
    }

    #[test]
    fn run_propagates_a_loop_error() {
        MOCK.with(|m| {
            *m.borrow_mut() = Mock {
                form: None,
                notice: None,
                result: Err("read failed"),
            }
        });
        assert_eq!(
            run(&Term::stdout(), "/tmp/projects", None, None)
                .unwrap_err()
                .to_string(),
            "read failed"
        );
    }
}

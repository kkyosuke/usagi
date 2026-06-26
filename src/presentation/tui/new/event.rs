use std::path::PathBuf;

use anyhow::Result;
use console::Key;
use console::Term;

use crate::presentation::tui::install_task;
use crate::presentation::tui::io::screen::{animated_read, FramePainter, KeyReader};
use crate::presentation::tui::widgets::dir_picker::{self, Choice, DirSource};

use super::state::{Field, FormState, NewProject};
use super::ui;

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

/// Runs the New Project screen against the given terminal and key source until
/// the user submits, goes back, or quits. Assumes the alternate screen is
/// already active (it is owned by the caller).
///
/// `default_location` pre-fills the Location field with the base directory new
/// projects are created under; the user can edit it before submitting.
///
/// `dir_source` backs the directory browser opened with Space on a directory
/// field (Location in Clone mode, the path in Existing mode).
pub fn event_loop(
    term: &Term,
    reader: &mut dyn KeyReader,
    default_location: &str,
    dir_source: &dyn DirSource,
) -> Result<Outcome> {
    let mut state = FormState::new();
    state.set_location(default_location);
    let mut notice: Option<String> = None;
    let mut painter = FramePainter::new();

    loop {
        let (height, width) = term.size();
        let frame = ui::render_frame(height as usize, width as usize, &state, notice.as_deref());
        painter.paint(term, frame)?;

        let key = match animated_read(reader, term, &mut painter, &install_task::handle()) {
            Ok(key) => key,
            // An interrupted read (e.g. a delivered signal) means quit.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return Ok(Outcome::Quit),
            Err(e) => return Err(anyhow::Error::from(e).context("Failed to read key")),
        };

        match key {
            Key::Escape => return Ok(Outcome::Back),
            // `Ctrl-C` / `Ctrl-Q` (the bare `0x11`) quit the app from here too.
            Key::CtrlC | Key::Char('\u{0011}') => return Ok(Outcome::Quit),
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
            // On the mode selector, ←/→ both flip between the two creation modes;
            // on a text field they move the caret instead, so a field can be
            // edited mid-string. A single arm handles both arrows: with only two
            // modes either direction toggles, and folding them avoids the
            // duplicate-but-distinct branches a third mode would silently break.
            Key::ArrowLeft | Key::ArrowRight if state.focus() == Field::Mode => {
                state.toggle_mode();
                notice = None;
            }
            Key::ArrowLeft => state.cursor_left(),
            Key::ArrowRight => state.cursor_right(),
            Key::Home => state.cursor_home(),
            Key::End => state.cursor_end(),
            Key::Backspace => {
                state.backspace();
                notice = None;
            }
            Key::Del => {
                state.delete_forward();
                notice = None;
            }
            // Space on a directory field opens the browser instead of typing a
            // space; on any other field it is an ordinary character.
            Key::Char(' ') if state.focus_is_directory() => {
                let start = resolve_start(state.directory_field_value(), default_location);
                match dir_picker::event_loop(term, reader, dir_source, &start)? {
                    Choice::Selected(path) => {
                        state.set_directory_field(&path.to_string_lossy());
                    }
                    Choice::Cancelled => {}
                    Choice::Quit => return Ok(Outcome::Quit),
                }
                // The browser modal drew over the form; force a full repaint.
                painter.reset();
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

/// The directory the browser should start in: the field's current value if set,
/// otherwise the default location, falling back to the filesystem root.
fn resolve_start(value: &str, fallback: &str) -> PathBuf {
    let value = value.trim();
    if !value.is_empty() {
        return PathBuf::from(value);
    }
    let fallback = fallback.trim();
    if !fallback.is_empty() {
        return PathBuf::from(fallback);
    }
    PathBuf::from("/")
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn type_keys(s: &str) -> Vec<io::Result<Key>> {
        s.chars().map(|c| Ok(Key::Char(c))).collect()
    }

    /// A directory source that lists the same two children for any directory,
    /// enough to drive the browser in the integration tests.
    struct FakeDirs;

    impl DirSource for FakeDirs {
        fn entries(&self, _dir: &std::path::Path) -> std::result::Result<Vec<String>, String> {
            Ok(vec!["projects".to_string(), "docs".to_string()])
        }
    }

    #[test]
    fn escape_returns_back() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Escape)]);
        assert!(matches!(
            event_loop(&term, &mut reader, "/base", &FakeDirs).unwrap(),
            Outcome::Back
        ));
    }

    #[test]
    fn ctrl_c_returns_quit() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::CtrlC)]);
        assert!(matches!(
            event_loop(&term, &mut reader, "/base", &FakeDirs).unwrap(),
            Outcome::Quit
        ));
    }

    #[test]
    fn ctrl_q_returns_quit() {
        // `Ctrl-Q` (the bare `0x11`) is the global quit chord on this screen too.
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Char('\u{0011}'))]);
        assert!(matches!(
            event_loop(&term, &mut reader, "/base", &FakeDirs).unwrap(),
            Outcome::Quit
        ));
    }

    #[test]
    fn enter_with_valid_url_submits() {
        let term = Term::stdout();
        // Tab off the mode selector onto the URL field before typing.
        let mut keys = vec![Ok(Key::Tab)];
        keys.extend(type_keys("https://github.com/owner/repo.git"));
        keys.push(Ok(Key::Enter));
        let mut reader = ScriptedReader::new(keys);
        // The pre-filled location lets validation succeed without editing it.
        let outcome = event_loop(&term, &mut reader, "/base", &FakeDirs).unwrap();
        assert!(matches!(
            &outcome,
            Outcome::Submitted(NewProject::Clone(spec))
                if spec.directory == "repo"
                    && spec.url.as_str() == "https://github.com/owner/repo.git"
                    && spec.location == std::path::Path::new("/base")
        ));
    }

    #[test]
    fn arrow_switches_to_existing_mode_and_submits_a_directory() {
        let term = Term::stdout();
        // Right toggles to the Existing mode, Tab focuses the Path field, then
        // typing a directory and Enter submits it.
        let mut keys = vec![Ok(Key::ArrowRight), Ok(Key::Tab)];
        keys.extend(type_keys("/home/me/my-app"));
        keys.push(Ok(Key::Enter));
        let mut reader = ScriptedReader::new(keys);
        let outcome = event_loop(&term, &mut reader, "/base", &FakeDirs).unwrap();
        assert!(matches!(
            &outcome,
            Outcome::Submitted(NewProject::Existing(spec))
                if spec.name == "my-app"
                    && spec.path == std::path::Path::new("/home/me/my-app")
        ));
    }

    #[test]
    fn enter_with_invalid_url_shows_notice_then_back() {
        let term = Term::stdout();
        // Enter on an empty form fails validation (notice), then Escape goes back.
        let mut reader = ScriptedReader::new(vec![Ok(Key::Enter), Ok(Key::Escape)]);
        assert!(matches!(
            event_loop(&term, &mut reader, "/base", &FakeDirs).unwrap(),
            Outcome::Back
        ));
    }

    #[test]
    fn navigation_and_editing_keys_are_handled() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![
            Ok(Key::Tab),        // focus_next (Mode -> Url)
            Ok(Key::ArrowDown),  // focus_next (Url -> Location)
            Ok(Key::BackTab),    // focus_prev (Location -> Url)
            Ok(Key::ArrowUp),    // focus_prev (Url -> Mode)
            Ok(Key::Tab),        // Mode -> Url, now on a text field
            Ok(Key::Char('a')),  // insert
            Ok(Key::Char('b')),  // insert -> "ab"
            Ok(Key::Home),       // caret to the start
            Ok(Key::ArrowRight), // caret between 'a' and 'b' (cursor_right)
            Ok(Key::ArrowLeft),  // caret before 'a' (cursor_left)
            Ok(Key::Del),        // forward-delete 'a' -> "b"
            Ok(Key::End),        // caret to the end
            Ok(Key::Backspace),  // delete -> ""
            Ok(Key::Insert),     // unhandled: exercises the `_` arm
            Ok(Key::Escape),     // back
        ]);
        assert!(matches!(
            event_loop(&term, &mut reader, "/base", &FakeDirs).unwrap(),
            Outcome::Back
        ));
    }

    #[test]
    fn arrows_on_the_mode_selector_toggle_the_mode() {
        let term = Term::stdout();
        // On the Mode selector (the default focus) ←/→ switch the creation mode;
        // ArrowRight then ArrowLeft toggles to Existing and back to Clone.
        let mut keys = vec![Ok(Key::ArrowRight), Ok(Key::ArrowLeft), Ok(Key::Tab)];
        keys.extend(type_keys("https://github.com/owner/repo.git"));
        keys.push(Ok(Key::Enter));
        let mut reader = ScriptedReader::new(keys);
        // Back in Clone mode, the URL submits as a clone.
        let outcome = event_loop(&term, &mut reader, "/base", &FakeDirs).unwrap();
        assert!(matches!(&outcome, Outcome::Submitted(NewProject::Clone(_))));
    }

    #[test]
    fn interrupted_read_returns_quit() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "interrupted",
        ))]);
        assert!(matches!(
            event_loop(&term, &mut reader, "/base", &FakeDirs).unwrap(),
            Outcome::Quit
        ));
    }

    #[test]
    fn unexpected_read_error_is_propagated() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Err(io::Error::other("boom"))]);
        let err = event_loop(&term, &mut reader, "/base", &FakeDirs).unwrap_err();
        assert!(err.to_string().contains("Failed to read key"));
    }

    #[test]
    fn space_on_the_path_field_browses_and_fills_the_chosen_directory() {
        let term = Term::stdout();
        // Switch to Existing, focus the Path field, open the browser with Space,
        // pick the current directory (the "/base" default) with Enter, then
        // submit the form with a second Enter.
        let mut reader = ScriptedReader::new(vec![
            Ok(Key::ArrowRight), // -> Existing mode
            Ok(Key::Tab),        // focus the Path field
            Ok(Key::Char(' ')),  // open the directory browser
            Ok(Key::Enter),      // browser: select "/base"
            Ok(Key::Enter),      // form: submit
        ]);
        let outcome = event_loop(&term, &mut reader, "/base", &FakeDirs).unwrap();
        // The picked directory fills Path, and the name is derived from it.
        assert!(matches!(
            &outcome,
            Outcome::Submitted(NewProject::Existing(spec))
                if spec.path == std::path::Path::new("/base") && spec.name == "base"
        ));
    }

    #[test]
    fn space_on_the_location_field_opens_the_browser_and_cancel_leaves_the_form() {
        let term = Term::stdout();
        // Clone mode: focus Location, open the browser, cancel it with Esc, then
        // leave the form with Esc.
        let mut reader = ScriptedReader::new(vec![
            Ok(Key::Tab),       // Mode -> Url
            Ok(Key::Tab),       // Url -> Location (a directory field)
            Ok(Key::Char(' ')), // open the browser
            Ok(Key::Escape),    // browser: cancel
            Ok(Key::Escape),    // form: back
        ]);
        assert!(matches!(
            event_loop(&term, &mut reader, "/base", &FakeDirs).unwrap(),
            Outcome::Back
        ));
    }

    #[test]
    fn quitting_the_browser_quits_the_screen() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![
            Ok(Key::ArrowRight), // -> Existing mode
            Ok(Key::Tab),        // focus the Path field
            Ok(Key::Char(' ')),  // open the browser
            Ok(Key::CtrlC),      // browser: quit
        ]);
        assert!(matches!(
            event_loop(&term, &mut reader, "/base", &FakeDirs).unwrap(),
            Outcome::Quit
        ));
    }

    #[test]
    fn space_on_a_non_directory_field_types_a_space() {
        let term = Term::stdout();
        // On the URL field Space is an ordinary character (the browser guard is
        // false), so the form keeps running until Escape.
        let mut reader = ScriptedReader::new(vec![
            Ok(Key::Tab),       // focus the URL field
            Ok(Key::Char(' ')), // typed, not a browser trigger
            Ok(Key::Escape),
        ]);
        assert!(matches!(
            event_loop(&term, &mut reader, "/base", &FakeDirs).unwrap(),
            Outcome::Back
        ));
    }

    #[test]
    fn resolve_start_prefers_value_then_fallback_then_root() {
        assert_eq!(resolve_start("/here", "/base"), PathBuf::from("/here"));
        // A blank field falls back to the default location.
        assert_eq!(resolve_start("   ", "/base"), PathBuf::from("/base"));
        // Both blank: the filesystem root.
        assert_eq!(resolve_start("", "  "), PathBuf::from("/"));
    }
}

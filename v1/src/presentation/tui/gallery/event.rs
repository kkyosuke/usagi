use std::io;

use anyhow::Result;
use console::Term;

use crate::presentation::tui::install_task;
use crate::presentation::tui::io::screen::{FramePainter, KeyReader};

use super::ui::{self, Variation};

/// Plays `variation` on a loop until the user presses a key. Each frame is
/// painted and then a `ANIM_TICK` read either returns a key (exit) or times out
/// (advance to the next frame), so the animation runs at a steady cadence while
/// staying responsive. `Ctrl+C` arrives as a key and an interrupted read is
/// treated as a key too, so either quits cleanly and the alternate-screen guard
/// restores the terminal. Assumes the alternate screen is already active.
pub fn event_loop(term: &Term, variation: Variation, reader: &mut dyn KeyReader) -> Result<()> {
    let mut painter = FramePainter::new();
    let mut frame = 0usize;

    loop {
        let (height, width) = term.size();
        let lines = ui::render_frame(variation, frame, height as usize, width as usize);
        painter.paint(term, lines)?;

        match reader.read_key_timeout(install_task::ANIM_TICK) {
            // Any key (including Ctrl+C) ends the gallery.
            Ok(Some(_)) => return Ok(()),
            // A tick with no key: advance the animation a frame and keep playing.
            Ok(None) => frame = frame.wrapping_add(1),
            // An interrupted read (signal) ends the gallery too.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(()),
            Err(e) => return Err(anyhow::Error::from(e).context("Failed to read key")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use console::Key;
    use std::collections::VecDeque;
    use std::time::Duration;

    /// A key source scripting `read_key_timeout` results so both the exit path (a
    /// key) and the animation path (ticks with no key) can be driven.
    struct ScriptedReader {
        timeouts: VecDeque<io::Result<Option<Key>>>,
    }

    impl ScriptedReader {
        fn new(timeouts: Vec<io::Result<Option<Key>>>) -> Self {
            Self {
                timeouts: timeouts.into(),
            }
        }
    }

    impl KeyReader for ScriptedReader {
        fn read_key(&mut self) -> io::Result<Key> {
            Ok(Key::Escape)
        }
        fn read_key_timeout(&mut self, _timeout: Duration) -> io::Result<Option<Key>> {
            // Default to a key so a test can never loop forever.
            self.timeouts.pop_front().unwrap_or(Ok(Some(Key::Escape)))
        }
    }

    fn run(timeouts: Vec<io::Result<Option<Key>>>) -> Result<()> {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(timeouts);
        event_loop(&term, Variation::Running, &mut reader)
    }

    #[test]
    fn a_key_press_ends_the_gallery() {
        assert!(run(vec![Ok(Some(Key::Char('q')))]).is_ok());
    }

    #[test]
    fn a_tick_advances_a_frame_then_a_key_exits() {
        // The first read times out (animate), the second yields a key (exit), so
        // the no-key branch is exercised.
        assert!(run(vec![Ok(None), Ok(Some(Key::Enter))]).is_ok());
    }

    #[test]
    fn an_interrupted_read_ends_the_gallery() {
        let interrupted = io::Error::new(io::ErrorKind::Interrupted, "interrupted");
        assert!(run(vec![Err(interrupted)]).is_ok());
    }

    #[test]
    fn an_unexpected_read_error_is_propagated() {
        let err = run(vec![Err(io::Error::other("boom"))]).unwrap_err();
        assert!(err.to_string().contains("Failed to read key"));
    }

    #[test]
    fn the_stub_reader_satisfies_the_blocking_read() {
        // The gallery only ever reads with a timeout, but `KeyReader` still
        // requires the blocking read; exercise the stub's so it stays honest.
        let mut reader = ScriptedReader::new(vec![]);
        assert_eq!(reader.read_key().unwrap(), Key::Escape);
    }
}

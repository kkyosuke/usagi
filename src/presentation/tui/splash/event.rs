use std::time::Duration;

use anyhow::Result;
use console::Term;

use crate::presentation::tui::install_task;
use crate::presentation::tui::screen::FramePainter;

use super::ui;

/// Plays the startup splash: the usagi runs across the screen for
/// [`ui::FRAMES`] frames, then the welcome menu takes over.
///
/// The splash is purely decorative and **never reads input** — it paces itself
/// by sleeping between frames. Doing so (rather than waiting on a key with a
/// timeout) keeps it off the same input path the welcome screen blocks on, so a
/// key pressed during the splash is left buffered and acted on by the menu
/// (type-ahead) instead of being raced for and lost. `sleep` is injected so the
/// loop is testable without real delays; production passes
/// [`std::thread::sleep`]. Assumes the alternate screen is already active (it is
/// owned by the orchestrator).
pub fn event_loop(term: &Term, sleep: &mut dyn FnMut(Duration)) -> Result<()> {
    let mut painter = FramePainter::new();

    for frame in 0..ui::FRAMES {
        let (height, width) = term.size();
        let lines = ui::render_frame(height as usize, width as usize, frame);
        painter.paint(term, lines)?;
        sleep(install_task::ANIM_TICK);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_paints_and_paces_every_frame() {
        // The splash paints each frame and waits one ANIM_TICK between them, so
        // it advances through the whole run exactly once.
        let term = Term::stdout();
        let mut ticks = 0usize;
        let mut sleep = |d: Duration| {
            assert_eq!(d, install_task::ANIM_TICK);
            ticks += 1;
        };
        assert!(event_loop(&term, &mut sleep).is_ok());
        assert_eq!(ticks, ui::FRAMES);
    }
}

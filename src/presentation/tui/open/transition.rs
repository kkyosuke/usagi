//! The mascot's flight from the Open screen into the home screen.
//!
//! When a project is opened the control flow is: select the project → hide the
//! project list → play this animation → show the home screen in 切替 (Switch).
//! The usagi the Open screen showed centred at the top lifts off and glides down
//! to the bottom-left corner — coming to rest just above where the home screen's
//! status line sits — over a now-blank screen (the list is hidden the instant the
//! flight begins). It is a brief, purely decorative flourish that ties the two
//! screens together.
//!
//! Like the startup [`splash`](super::super::splash), it **never reads input**:
//! it paces itself by sleeping between frames so a key pressed during the flight
//! is left buffered for the home screen rather than raced for and lost. `sleep`
//! is injected so the loop is testable without real delays.

use std::time::Duration;

use anyhow::Result;
use console::Term;

use crate::presentation::tui::install_task;
use crate::presentation::tui::screen::FramePainter;
use crate::presentation::tui::widgets;

/// Frames the mascot takes to glide from the Open header to the bottom-left
/// corner. At [`install_task::ANIM_TICK`] per frame this is a brief (~1.3s)
/// flourish before the home screen appears.
pub const FRAMES: usize = 12;

/// Where the mascot comes to rest: just above the bottom-left status line
/// (`● live terminal` and the other mode prompts sit on the frame's
/// second-to-last row, the footer below them), at a one-column left margin that
/// matches that line's own indent.
///
/// Returns the `(row, col)` of the rabbit's **top** line, so its bottom line
/// lands one row above the status line.
fn target_pos(height: usize) -> (usize, usize) {
    // The status line is the second-to-last row (`height - 2`); the rabbit spans
    // `rabbit_height` rows and rests one row above it.
    let bottom = height.saturating_sub(3);
    (bottom.saturating_sub(widgets::rabbit_height() - 1), 1)
}

/// The `(row, col)` of the mascot at `frame` of `frames`, interpolated linearly
/// from `start` to `end`. Frame `0` is exactly `start` and frame `frames - 1` is
/// exactly `end`, so the flight begins where the rabbit rested and finishes at
/// the corner.
fn interpolate(
    start: (usize, usize),
    end: (usize, usize),
    frame: usize,
    frames: usize,
) -> (usize, usize) {
    let denom = (frames - 1).max(1) as isize;
    let frame = frame.min(frames - 1) as isize;
    let lerp = |a: usize, b: usize| -> usize {
        let (a, b) = (a as isize, b as isize);
        (a + (b - a) * frame / denom).max(0) as usize
    };
    (lerp(start.0, end.0), lerp(start.1, end.1))
}

/// Draws the mascot onto a copy of `backdrop` with its top line at (`top`,
/// `col`), each rabbit row *replacing* the backdrop row it covers so the art
/// reads cleanly as it slides down the screen. Rabbit rows past the end of the
/// backdrop are dropped.
fn render(backdrop: &[String], top: usize, col: usize) -> Vec<String> {
    let mut frame = backdrop.to_vec();
    for (offset, line) in widgets::rabbit_lines_at(col).into_iter().enumerate() {
        if let Some(row) = frame.get_mut(top + offset) {
            *row = line;
        }
    }
    frame
}

/// Plays the mascot's flight from `start` (the `(row, col)` the Open screen drew
/// it at) to the bottom-left corner, painting over `backdrop`.
///
/// `backdrop` is the screen behind the rabbit during the flight — a blank frame,
/// so the project list disappears the instant the animation starts (only the
/// gliding rabbit shows). The mascot's resting rows are blanked defensively so it
/// leaves no ghost behind at its start. Paints through the caller's `painter`;
/// with a fresh painter the first frame clears whatever the Open screen left
/// behind (hiding the list), and the rabbit — at its start on frame 0 — then
/// descends frame by frame.
pub fn play(
    term: &Term,
    painter: &mut FramePainter,
    mut backdrop: Vec<String>,
    start: (usize, usize),
    sleep: &mut dyn FnMut(Duration),
) -> Result<()> {
    // Blank the mascot's resting rows so only the moving rabbit is ever on screen.
    for offset in 0..widgets::rabbit_height() {
        if let Some(row) = backdrop.get_mut(start.0 + offset) {
            *row = String::new();
        }
    }

    let (raw_height, raw_width) = term.size();
    let (height, _width) = widgets::normalize_size(raw_height as usize, raw_width as usize);
    let target = target_pos(height);

    for frame in 0..FRAMES {
        let (row, col) = interpolate(start, target, frame, FRAMES);
        painter.paint(term, render(&backdrop, row, col))?;
        sleep(install_task::ANIM_TICK);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backdrop(height: usize) -> Vec<String> {
        (0..height).map(|i| format!("row {i}")).collect()
    }

    #[test]
    fn target_rests_above_the_bottom_left_status_line() {
        // The rabbit's bottom row lands one row above the status line (the
        // second-to-last row), flush to a one-column left margin.
        let height = 24;
        let (top, col) = target_pos(height);
        assert_eq!(col, 1);
        // Bottom row = top + height_of_art - 1, one above the status row.
        assert_eq!(top + widgets::rabbit_height() - 1, height - 3);
    }

    #[test]
    fn target_never_underflows_a_tiny_terminal() {
        // A terminal too short for the corner clamps to the top rather than wrapping.
        assert_eq!(target_pos(1), (0, 1));
    }

    #[test]
    fn interpolate_pins_the_endpoints_and_moves_between_them() {
        let start = (4, 30);
        let end = (19, 1);
        // The first frame is exactly the start and the last exactly the end.
        assert_eq!(interpolate(start, end, 0, FRAMES), start);
        assert_eq!(interpolate(start, end, FRAMES - 1, FRAMES), end);
        // A middle frame sits strictly between the two — the rabbit has descended
        // and drifted left.
        let (row, col) = interpolate(start, end, FRAMES / 2, FRAMES);
        assert!(row > start.0 && row < end.0);
        assert!(col < start.1 && col > end.1);
        // A frame past the run clamps to the end rather than overshooting.
        assert_eq!(interpolate(start, end, FRAMES + 5, FRAMES), end);
    }

    #[test]
    fn interpolate_handles_a_single_frame_run() {
        // With one frame there is nothing to divide by; it yields the start.
        assert_eq!(interpolate((2, 2), (9, 9), 0, 1), (2, 2));
    }

    #[test]
    fn render_places_the_rabbit_over_the_backdrop_and_keeps_other_rows() {
        let backdrop = backdrop(24);
        let frame = render(&backdrop, 10, 5);
        // The three art rows are replaced by the mascot...
        let mascot = console::strip_ansi_codes(&frame[10..13].join("\n")).into_owned();
        assert!(mascot.contains("(='-')"));
        // The art is indented to the requested column.
        assert!(console::strip_ansi_codes(&frame[10]).starts_with(&" ".repeat(5)));
        // ...and the rows around it still show the backdrop.
        assert_eq!(frame[9], "row 9");
        assert_eq!(frame[13], "row 13");
    }

    #[test]
    fn render_drops_rabbit_rows_past_the_backdrop() {
        // Placing the rabbit at the very bottom keeps the frame the same length
        // (rows past the end are dropped, never appended).
        let backdrop = backdrop(5);
        let frame = render(&backdrop, 4, 0);
        assert_eq!(frame.len(), 5);
        // Only the first art row fits on the last backdrop row.
        assert!(console::strip_ansi_codes(&frame[4]).contains("(\\(\\"));
    }

    #[test]
    fn play_paints_and_paces_every_frame_and_blanks_the_resting_rows() {
        // The flight paints each frame once and sleeps one ANIM_TICK between them.
        let term = Term::stdout();
        let mut painter = FramePainter::new();
        let mut backdrop = backdrop(24);
        // Seed the mascot's resting rows so we can confirm they are blanked.
        let start = (4, 35);
        for offset in 0..widgets::rabbit_height() {
            backdrop[start.0 + offset] = "MASCOT".to_string();
        }
        let mut ticks = 0usize;
        let mut sleep = |d: Duration| {
            assert_eq!(d, install_task::ANIM_TICK);
            ticks += 1;
        };
        assert!(play(&term, &mut painter, backdrop, start, &mut sleep).is_ok());
        assert_eq!(ticks, FRAMES);
    }
}

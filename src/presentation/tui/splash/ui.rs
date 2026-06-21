use crate::presentation::tui::widgets;

/// Frames the splash plays before the welcome menu takes over. The usagi mascot
/// holds for the whole run; the `USAGI` title stays hidden for [`TITLE_DELAY`]
/// frames, then fades in over [`TITLE_FADE_STEPS`](widgets::TITLE_FADE_STEPS)
/// brightness steps and holds at full green-bold for [`TITLE_HOLD`] more frames.
/// At [`ANIM_TICK`](crate::presentation::tui::install_task::ANIM_TICK) per frame
/// this is a brief flash before the menu.
pub const FRAMES: usize = TITLE_DELAY + widgets::TITLE_FADE_STEPS + TITLE_HOLD;

const TITLE: &str = "USAGI";

/// Frames the mascot shows alone before the title begins to fade in.
const TITLE_DELAY: usize = 5;

/// Frames the fully faded-in title holds before the welcome menu takes over.
const TITLE_HOLD: usize = 4;

/// The title's fade step at `frame`: blank ([`step 0`](widgets::faded_title_line))
/// through the [`TITLE_DELAY`] frames the mascot shows alone, then one step per
/// frame up to the full green-bold title, which it holds at thereafter.
fn title_fade_step(frame: usize) -> usize {
    if frame < TITLE_DELAY {
        0
    } else {
        (frame - TITLE_DELAY + 1).min(widgets::TITLE_FADE_STEPS)
    }
}

/// Builds the splash frame for `frame` at a raw terminal size: the welcome
/// screen's usagi mascot above the centred `USAGI` title, the whole block centred
/// both vertically and horizontally — the same mascot and title the welcome menu
/// shows, so the splash hands off to it without a jump.
///
/// The mascot is identical on every frame; only the title animates. It is hidden
/// for the first [`TITLE_DELAY`] frames and then fades in (see
/// [`title_fade_step`]), so the title appears a beat after the mascot and
/// brightens into place. The title's row is always reserved (a blank line while
/// it is hidden), so nothing shifts as it appears.
pub fn render_frame(raw_height: usize, raw_width: usize, frame: usize) -> Vec<String> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);

    let mut body = widgets::rabbit_lines(width);
    body.push(String::new());
    body.push(widgets::faded_title_line(
        width,
        TITLE,
        title_fade_step(frame),
    ));

    let mut lines = Vec::with_capacity(height);
    let top_padding = height.saturating_sub(body.len()) / 2;
    for _ in 0..top_padding {
        lines.push(String::new());
    }
    lines.extend(body);
    while lines.len() < height {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_fade_step_holds_blank_then_fades_in_and_settles() {
        // The title is hidden (step 0) for the whole delay, then advances one step
        // per frame, and never exceeds the full-brightness final step.
        assert_eq!(title_fade_step(0), 0);
        assert_eq!(title_fade_step(TITLE_DELAY - 1), 0);
        // The first frame past the delay shows the first (dim) fade step.
        assert_eq!(title_fade_step(TITLE_DELAY), 1);
        assert_eq!(title_fade_step(TITLE_DELAY + 1), 2);
        // By the last frame the title is held at full brightness.
        assert_eq!(title_fade_step(FRAMES - 1), widgets::TITLE_FADE_STEPS);
    }

    #[test]
    fn render_frame_shows_the_mascot_and_hides_the_title_during_the_delay() {
        let frame = render_frame(24, 80, 0);
        assert_eq!(frame.len(), 24);
        let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
        // The welcome screen's mascot face is on screen from the first frame...
        assert!(joined.contains("(='-')"));
        // ...but the title has not begun to fade in yet.
        assert!(!joined.contains(TITLE));
    }

    #[test]
    fn render_frame_fades_the_title_in_after_the_mascot() {
        // Once the delay passes the title appears alongside the unchanging mascot.
        let frame = render_frame(24, 80, TITLE_DELAY);
        let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
        assert!(joined.contains("(='-')"));
        assert!(joined.contains(TITLE));
    }

    #[test]
    fn render_frame_keeps_the_mascot_fixed_while_the_title_animates() {
        // Only the title row changes between frames; the mascot rows are identical
        // before and after the title fades in, so nothing jumps.
        let before = render_frame(24, 80, 0);
        let after = render_frame(24, 80, FRAMES - 1);
        let mascot = |frame: &[String]| {
            frame
                .iter()
                .map(|l| console::strip_ansi_codes(l).into_owned())
                .filter(|l| l.contains("(='-')") || l.contains("(\\(\\"))
                .collect::<Vec<_>>()
        };
        assert_eq!(mascot(&before), mascot(&after));
        // The two frames still differ overall — the title faded in.
        assert_ne!(before, after);
    }

    #[test]
    fn render_frame_centers_the_body_vertically() {
        let frame = render_frame(40, 80, FRAMES - 1);
        assert_eq!(frame.len(), 40);
        // Leading blank rows centre the body; the title is somewhere in the middle.
        let top_padding = frame.iter().take_while(|l| l.is_empty()).count();
        assert!(top_padding > 0);
        assert!(frame.iter().any(|l| l.contains(TITLE)));
    }

    #[test]
    fn render_frame_substitutes_a_fallback_for_a_zero_size() {
        // A non-interactive zero size falls back to 80x24 rather than rendering
        // an empty frame.
        let frame = render_frame(0, 0, 0);
        assert_eq!(frame.len(), 24);
    }
}

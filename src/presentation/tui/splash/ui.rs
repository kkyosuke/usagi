use crate::presentation::tui::widgets;

/// Frames the splash plays: one full round trip of the running rabbit (out to
/// one edge and back). At [`ANIM_TICK`](crate::presentation::tui::install_task::ANIM_TICK)
/// per frame this lasts a couple of seconds before the welcome menu takes over.
pub const FRAMES: usize = 24;

const TITLE: &str = "USAGI";

/// The rabbit's horizontal offset and facing for `frame` of `total`, bouncing
/// across a `span`-column track: it runs from the left edge to the right
/// (`face_right`) over the first half of the splash, then back to the left over
/// the second half. A zero-width track keeps it resting at the left, facing
/// right.
fn position(frame: usize, total: usize, span: usize) -> (usize, bool) {
    if span == 0 || total < 2 {
        return (0, true);
    }
    let half = total / 2;
    if frame < half {
        // Outbound: 0 → span, facing right.
        (span * frame / half, true)
    } else {
        // Return leg: span → 0, facing left.
        let back = (frame - half).min(total - half);
        (span - span * back / (total - half), false)
    }
}

/// Builds the splash frame for `frame` at a raw terminal size: the running usagi
/// bobbing across the screen above the centred `USAGI` title, the whole block
/// centred vertically. The motion (the edge-to-edge bounce and the per-frame
/// hop) is derived from `frame`, so painting successive frames animates the run.
pub fn render_frame(raw_height: usize, raw_width: usize, frame: usize) -> Vec<String> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let span = width.saturating_sub(widgets::running_rabbit_width());
    let (col, face_right) = position(frame, FRAMES, span);
    // Hop every frame so the rabbit bounds as it travels.
    let airborne = frame.is_multiple_of(2);

    let mut body = widgets::running_rabbit(col, face_right, airborne);
    body.push(String::new());
    body.push(widgets::title_line(width, TITLE));

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
    fn position_runs_out_to_the_right_then_back_left() {
        let span = 60;
        // Starts at the left, facing right.
        assert_eq!(position(0, FRAMES, span), (0, true));
        // Reaches the right edge at the halfway frame and flips to face left.
        let (col, face_right) = position(FRAMES / 2, FRAMES, span);
        assert_eq!(col, span);
        assert!(!face_right);
        // Heads back toward the left over the return leg.
        let (later, _) = position(FRAMES - 1, FRAMES, span);
        assert!(later < span);
    }

    #[test]
    fn position_rests_at_the_left_when_there_is_no_room_to_run() {
        // A zero-width track (terminal no wider than the sprite) keeps the rabbit
        // put rather than dividing by zero.
        assert_eq!(position(5, FRAMES, 0), (0, true));
    }

    #[test]
    fn render_frame_fills_the_terminal_and_shows_the_title() {
        let frame = render_frame(24, 80, 0);
        assert_eq!(frame.len(), 24);
        let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
        assert!(joined.contains(TITLE));
        // The running rabbit's face is on screen.
        assert!(joined.contains('ㅅ'));
    }

    #[test]
    fn render_frame_animates_across_frames() {
        // Different frames place the rabbit differently, so successive paints move.
        let a = console::strip_ansi_codes(&render_frame(24, 80, 1).join("\n")).into_owned();
        let b = console::strip_ansi_codes(&render_frame(24, 80, 2).join("\n")).into_owned();
        assert_ne!(a, b);
    }

    #[test]
    fn render_frame_centers_the_body_vertically() {
        let frame = render_frame(40, 80, 0);
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

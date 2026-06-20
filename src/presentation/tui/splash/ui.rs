use crate::presentation::tui::widgets;

/// Frames the splash plays: the warren grows from one usagi to a full
/// [`MAX_RABBITS`], then the welcome menu takes over. At
/// [`ANIM_TICK`](crate::presentation::tui::install_task::ANIM_TICK) per frame
/// this is a brief flash before the menu — shorter than the old edge-to-edge run.
pub const FRAMES: usize = 16;

const TITLE: &str = "USAGI";

/// Frames between each new usagi joining the warren as the splash plays.
const GROW: usize = 2;

/// The warren the splash fills to before the welcome menu takes over.
const MAX_RABBITS: usize = 8;

/// How many usagi are shown at `frame`: the warren starts at one and a new rabbit
/// joins every [`GROW`] frames, capped at [`MAX_RABBITS`]. The line is anchored
/// to the left edge by
/// [`multiplying_rabbits`](widgets::multiplying_rabbits), so each arrival extends
/// it rightward without nudging the rabbits already on screen.
fn rabbit_count(frame: usize) -> usize {
    (1 + frame / GROW).min(MAX_RABBITS)
}

/// Builds the splash frame for `frame` at a raw terminal size: the "multiplying"
/// usagi (the `usagi run 2` animation) filling the warren from the left edge,
/// above the centred `USAGI` title, the whole block centred vertically. The count
/// is derived from `frame`, so painting successive frames grows the warren. The
/// rabbit rows are a fixed three lines and the first usagi holds the left edge, so
/// nothing shifts vertically or horizontally as the warren grows.
pub fn render_frame(raw_height: usize, raw_width: usize, frame: usize) -> Vec<String> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);

    let mut body = widgets::multiplying_rabbits(rabbit_count(frame));
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
    fn rabbit_count_grows_from_one_and_caps() {
        // The warren starts at a single usagi, grows a rabbit every `GROW` frames,
        // and never exceeds the cap.
        assert_eq!(rabbit_count(0), 1);
        assert!(rabbit_count(GROW) > rabbit_count(0));
        assert_eq!(rabbit_count(usize::MAX / 2), MAX_RABBITS);
        assert_eq!(rabbit_count(FRAMES - 1), MAX_RABBITS);
    }

    #[test]
    fn render_frame_fills_the_terminal_and_shows_the_title() {
        let frame = render_frame(24, 80, 0);
        assert_eq!(frame.len(), 24);
        let joined = console::strip_ansi_codes(&frame.join("\n")).into_owned();
        assert!(joined.contains(TITLE));
        // A multiplying usagi's face is on screen.
        assert!(joined.contains("(｡･-･)"));
    }

    #[test]
    fn render_frame_animates_across_frames() {
        // Later frames hold more usagi, so successive paints differ.
        let a = console::strip_ansi_codes(&render_frame(24, 80, 0).join("\n")).into_owned();
        let b = console::strip_ansi_codes(&render_frame(24, 80, GROW).join("\n")).into_owned();
        assert_ne!(a, b);
        assert!(b.matches("(｡･-･)").count() > a.matches("(｡･-･)").count());
    }

    #[test]
    fn render_frame_keeps_the_first_usagi_at_the_left_edge() {
        // The warren is anchored left: the face row begins flush at column zero on
        // both an early and a later frame, so growth never shifts it sideways.
        for frame in [0usize, GROW * 3] {
            let painted = render_frame(24, 80, frame);
            let face_row = painted
                .iter()
                .map(|l| console::strip_ansi_codes(l).into_owned())
                .find(|l| l.contains("(｡･-･)"))
                .expect("a face row is painted");
            assert!(face_row.starts_with("(｡･-･)"), "frame {frame}: flush left");
        }
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

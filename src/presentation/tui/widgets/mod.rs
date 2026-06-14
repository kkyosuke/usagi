//! Shared TUI rendering primitives used across screens.
//!
//! Layout maths (centring, size normalisation) and the common visual elements
//! — the usagi mascot, screen titles, dimmed subtitles/footers, and the modal
//! box that overlays a screen — live here so every screen renders them
//! consistently. Stateful, reusable widgets (e.g. the searchable [`picker`])
//! live in submodules.

pub mod dir_picker;
pub mod picker;

use console::style;

/// The usagi mascot artwork (raw, unstyled lines).
const RABBIT: [&str; 3] = ["  (\\(\\ ", " (='-') ", " o(_(\")(\")"];

/// Left padding that horizontally centres content of `content_width` columns
/// within a terminal `term_width` columns wide. Saturates to 0 when the content
/// is wider than the terminal.
pub fn centered_padding(term_width: usize, content_width: usize) -> usize {
    term_width.saturating_sub(content_width) / 2
}

/// Normalises a raw terminal size, substituting an 80x24 fallback for the
/// zeroes that non-interactive environments report.
pub fn normalize_size(height: usize, width: usize) -> (usize, usize) {
    let height = if height == 0 { 24 } else { height };
    let width = if width == 0 { 80 } else { width };
    (height, width)
}

/// Centres a single line of `text` by left-padding it with spaces.
fn centered(width: usize, text: &str) -> String {
    let padding = " ".repeat(centered_padding(width, text.chars().count()));
    format!("{padding}{text}")
}

/// The usagi mascot, centred for the terminal width and styled magenta-bold.
///
/// The whole block shares a single padding so the art stays aligned.
pub fn rabbit_lines(width: usize) -> Vec<String> {
    let rabbit_width = RABBIT.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let padding = " ".repeat(centered_padding(width, rabbit_width));
    RABBIT
        .iter()
        .map(|line| {
            style(format!("{padding}{line}"))
                .magenta()
                .bold()
                .to_string()
        })
        .collect()
}

/// A centred, green-bold screen title.
pub fn title_line(width: usize, title: &str) -> String {
    style(centered(width, title)).green().bold().to_string()
}

/// A centred, dimmed line — used for subtitles and footers.
pub fn dim_line(width: usize, text: &str) -> String {
    style(centered(width, text)).dim().to_string()
}

/// A left/right value chooser — the shared rendering primitive for every
/// settings field that cycles through choices.
///
/// The value is always wrapped in chevrons — `< Dark >` — so every field reads
/// as a left/right selector and the chevrons line up in a single column down
/// the screen. Colour conveys state: the `focused` row is bright (cyan-bold),
/// the rest are dimmed.
///
/// `changed` marks a value that differs from what is saved on disk: it is
/// painted yellow (taking priority over the focused/idle colours) so unsaved
/// edits stand out at a glance.
pub fn chooser(value: &str, focused: bool, changed: bool) -> String {
    let paint = |text: &str| {
        let styled = style(text.to_string());
        if changed {
            styled.yellow().bold()
        } else if focused {
            styled.cyan().bold()
        } else {
            styled.dim()
        }
        .to_string()
    };

    format!("{} {} {}", paint("<"), paint(value), paint(">"))
}

/// Wraps `lines` in a single-bordered box `inner_width` columns wide, with
/// `title` embedded in the top border.
///
/// Each content line is padded — by *display* width, so text carrying ANSI
/// styling still aligns — to `inner_width`, with one space of breathing room on
/// each side. The returned rows are not yet placed; [`render_modal`] centres
/// them. A shared primitive so every modal dialog shares one frame.
pub fn boxed(title: &str, inner_width: usize, lines: &[String]) -> Vec<String> {
    // Columns between the two corner glyphs: the content area plus one space of
    // padding on each side.
    let span = inner_width + 2;
    let label = if title.is_empty() {
        String::new()
    } else {
        format!("─ {title} ")
    };
    let label_width = console::measure_text_width(&label);
    let top = format!("┌{label}{}┐", "─".repeat(span.saturating_sub(label_width)));
    let bottom = format!("└{}┘", "─".repeat(span));

    let mut out = Vec::with_capacity(lines.len() + 2);
    out.push(top);
    for line in lines {
        let pad = inner_width.saturating_sub(console::measure_text_width(line));
        out.push(format!("│ {line}{} │", " ".repeat(pad)));
    }
    out.push(bottom);
    out
}

/// Renders `body` inside a centred [`boxed`] modal for a raw terminal size.
///
/// The box is centred both horizontally and vertically over an otherwise blank
/// frame, mirroring how the full-screen screens build their frames so the event
/// loop can clear and redraw it the same way.
pub fn render_modal(
    raw_height: usize,
    raw_width: usize,
    title: &str,
    inner_width: usize,
    body: &[String],
) -> Vec<String> {
    let (height, width) = normalize_size(raw_height, raw_width);
    let box_lines = boxed(title, inner_width, body);
    // The box is `inner_width` plus the two spaces of padding and two borders.
    let pad = " ".repeat(centered_padding(width, inner_width + 4));

    let mut lines = Vec::with_capacity(height);
    let top_padding = height.saturating_sub(box_lines.len()) / 2;
    for _ in 0..top_padding {
        lines.push(String::new());
    }
    for line in &box_lines {
        lines.push(format!("{pad}{line}"));
    }
    while lines.len() < height {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centered_padding_centers_content() {
        assert_eq!(centered_padding(80, 10), 35);
        assert_eq!(centered_padding(81, 10), 35);
    }

    #[test]
    fn centered_padding_handles_narrow_terminal() {
        assert_eq!(centered_padding(5, 10), 0);
    }

    #[test]
    fn normalize_size_substitutes_fallbacks_for_zero() {
        assert_eq!(normalize_size(0, 0), (24, 80));
    }

    #[test]
    fn normalize_size_keeps_nonzero_values() {
        assert_eq!(normalize_size(30, 100), (30, 100));
    }

    #[test]
    fn rabbit_lines_are_three_centered_mascot_rows() {
        let lines = rabbit_lines(80);
        assert_eq!(lines.len(), 3);
        // The mascot face appears, and the block is indented (centred).
        assert!(lines.iter().any(|l| l.contains("(='-')")));
        assert!(lines[0].starts_with(' '));
    }

    #[test]
    fn title_line_contains_the_title() {
        assert!(title_line(80, "USAGI").contains("USAGI"));
    }

    #[test]
    fn dim_line_contains_the_text() {
        assert!(dim_line(80, "hint").contains("hint"));
    }

    #[test]
    fn chooser_always_brackets_the_value() {
        // Chevrons show whether focused or not, so every field reads as a
        // selector and the chevrons align down the column.
        for focused in [true, false] {
            let rendered = chooser("Dark", focused, false);
            assert!(rendered.contains("Dark"));
            assert!(rendered.contains('<'));
            assert!(rendered.contains('>'));
        }
    }

    #[test]
    fn chooser_keeps_the_value_aligned_across_focus() {
        // Focus changes only the colour, not the layout, so the visible width is
        // identical and the column never jumps.
        let focused = console::strip_ansi_codes(&chooser("On", true, false)).into_owned();
        let idle = console::strip_ansi_codes(&chooser("On", false, false)).into_owned();
        assert_eq!(focused, idle);
    }

    #[test]
    fn chooser_marks_changed_values() {
        // A changed value still renders its text; the colour difference is what
        // signals the unsaved edit, and it applies whether focused or not.
        assert!(chooser("Gemini", true, true).contains("Gemini"));
        assert!(chooser("Gemini", false, true).contains("Gemini"));
    }

    #[test]
    fn boxed_frames_the_lines_with_a_title_and_borders() {
        let lines = boxed("Title", 10, &["hi".to_string(), "world".to_string()]);
        // Two content rows plus the top and bottom borders.
        assert_eq!(lines.len(), 4);
        assert!(lines[0].starts_with('┌'));
        assert!(lines[0].contains("Title"));
        assert!(lines[0].ends_with('┐'));
        assert!(lines.last().unwrap().starts_with('└'));
        assert!(lines.last().unwrap().ends_with('┘'));
        // Content rows are bordered and equal width (padded to the inner width).
        assert!(lines[1].contains("hi"));
        assert!(lines[2].contains("world"));
        assert_eq!(
            console::measure_text_width(&lines[1]),
            console::measure_text_width(&lines[2]),
        );
    }

    #[test]
    fn boxed_without_a_title_is_all_dashes_on_top() {
        let lines = boxed("", 4, &["x".to_string()]);
        // No title segment: the top border is corners plus a run of dashes.
        assert!(lines[0].starts_with('┌'));
        assert!(lines[0].contains('─'));
        assert!(!lines[0].contains(' '));
    }

    #[test]
    fn render_modal_centers_a_box_over_a_full_frame() {
        let frame = render_modal(24, 80, "Pick", 20, &["row".to_string()]);
        assert_eq!(frame.len(), 24);
        let joined = frame.join("\n");
        assert!(joined.contains("Pick"));
        assert!(joined.contains("row"));
        // The box is offset from the left edge (horizontally centred).
        let box_row = frame.iter().find(|l| l.contains("Pick")).unwrap();
        assert!(box_row.starts_with(' '));
        // Blank rows above the box (vertically centred).
        assert!(frame[0].is_empty());
    }
}

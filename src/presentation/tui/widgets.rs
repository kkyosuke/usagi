//! Shared TUI rendering primitives used across screens.
//!
//! Layout maths (centring, size normalisation) and the common visual elements
//! — the usagi mascot, screen titles, dimmed subtitles/footers — live here so
//! every screen renders them consistently.

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
/// When `focused`, the value is wrapped in chevrons — `< Dark >` — to signal it
/// can be cycled with ←/→. When not focused it shows the value plainly, padded
/// with the same two columns the chevrons occupy so the value never shifts as
/// focus moves between rows.
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

    if focused {
        format!("{} {} {}", paint("<"), paint(value), paint(">"))
    } else {
        // Two spaces each side mirror the `< ` / ` >` of the focused form.
        paint(&format!("  {value}  "))
    }
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
    fn chooser_brackets_the_value_only_when_focused() {
        let focused = chooser("Dark", true, false);
        assert!(focused.contains("Dark"));
        assert!(focused.contains('<'));
        assert!(focused.contains('>'));

        let idle = chooser("Dark", false, false);
        assert!(idle.contains("Dark"));
        assert!(!idle.contains('<'));
        assert!(!idle.contains('>'));
    }

    #[test]
    fn chooser_keeps_the_value_aligned_across_focus() {
        // The value plus its surrounding two columns occupies the same width
        // whether focused (`< v >`) or not (`  v  `), so the column never jumps.
        let focused = console::strip_ansi_codes(&chooser("On", true, false)).into_owned();
        let idle = console::strip_ansi_codes(&chooser("On", false, false)).into_owned();
        assert_eq!(focused.chars().count(), idle.chars().count());
    }

    #[test]
    fn chooser_marks_changed_values() {
        // A changed value still renders its text; the colour difference is what
        // signals the unsaved edit, and it applies whether focused or not.
        assert!(chooser("Gemini", true, true).contains("Gemini"));
        assert!(chooser("Gemini", false, true).contains("Gemini"));
    }
}

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

/// An on/off toggle, rendering the active state brightly and the inactive one
/// dimmed: e.g. `On · off` when on, `on · Off` when off.
///
/// A shared rendering primitive for boolean settings — keeps every toggle
/// looking the same wherever it is used.
pub fn toggle(on: bool) -> String {
    let on_label = if on {
        style("On").green().bold().to_string()
    } else {
        style("on").dim().to_string()
    };
    let off_label = if on {
        style("off").dim().to_string()
    } else {
        style("Off").red().bold().to_string()
    };
    format!("{on_label} · {off_label}")
}

/// An inline single-choice selector: each option is shown, with the selected
/// one bracketed and highlighted and the rest dimmed — e.g. `[Claude]  Gemini`.
///
/// A shared rendering primitive for enum settings (theme, agent CLI, …).
pub fn select(options: &[&str], selected: usize) -> String {
    options
        .iter()
        .enumerate()
        .map(|(i, option)| {
            if i == selected {
                style(format!("[{option}]")).cyan().bold().to_string()
            } else {
                style(format!(" {option} ")).dim().to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
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
    fn toggle_shows_both_states_in_either_position() {
        let on = toggle(true);
        assert!(on.contains("On"));
        assert!(on.contains("off"));
        let off = toggle(false);
        assert!(off.contains("on"));
        assert!(off.contains("Off"));
    }

    #[test]
    fn select_brackets_only_the_selected_option() {
        let rendered = select(&["Claude", "Gemini"], 0);
        assert!(rendered.contains("[Claude]"));
        assert!(rendered.contains("Gemini"));
        assert!(!rendered.contains("[Gemini]"));

        let rendered = select(&["Claude", "Gemini"], 1);
        assert!(rendered.contains("[Gemini]"));
        assert!(!rendered.contains("[Claude]"));
    }
}

use console::style;

use super::state::{Field, FormState};

const RABBIT_LINES: [&str; 3] = ["  (\\(\\ ", " (='-') ", " o(_(\")(\")"];

const TITLE: &str = "New Project";
const SUBTITLE: &str = "Clone a Git repository into a new workspace";

/// Fixed width of the form block; the whole block is centred in the terminal.
const BLOCK_WIDTH: usize = 52;

/// Caret shown at the end of the focused input field.
const CARET: &str = "▏";

/// Left padding that centres content of the given width in the terminal.
fn centered_padding(term_width: usize, content_width: usize) -> usize {
    term_width.saturating_sub(content_width) / 2
}

/// Normalises a raw terminal size, substituting fallbacks for the zeroes that
/// non-interactive environments report.
fn normalize_size(height: usize, width: usize) -> (usize, usize) {
    let height = if height == 0 { 24 } else { height };
    let width = if width == 0 { 80 } else { width };
    (height, width)
}

/// Builds the centred mascot, title, and subtitle lines.
fn header_lines(height: usize, width: usize) -> Vec<String> {
    let mut lines = Vec::new();

    let top_padding = if height > RABBIT_LINES.len() + 14 {
        2
    } else {
        1
    };
    for _ in 0..top_padding {
        lines.push(String::new());
    }

    let rabbit_width = RABBIT_LINES
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0);
    let rabbit_pad = " ".repeat(centered_padding(width, rabbit_width));
    for line in RABBIT_LINES {
        lines.push(
            style(format!("{rabbit_pad}{line}"))
                .magenta()
                .bold()
                .to_string(),
        );
    }

    lines.push(String::new());
    let title_pad = " ".repeat(centered_padding(width, TITLE.chars().count()));
    lines.push(
        style(format!("{title_pad}{TITLE}"))
            .green()
            .bold()
            .to_string(),
    );
    let subtitle_pad = " ".repeat(centered_padding(width, SUBTITLE.chars().count()));
    lines.push(format!("{subtitle_pad}{}", style(SUBTITLE).dim()));
    lines.push(String::new());

    lines
}

/// Builds one input row: a `>` cursor for the focused field, the value (or a
/// dim placeholder when empty), and a caret on the focused field.
fn input_line(block_pad: &str, value: &str, placeholder: &str, focused: bool) -> String {
    let marker = if focused {
        style(">").red().bold().to_string()
    } else {
        " ".to_string()
    };

    let body = if value.is_empty() {
        if focused {
            // Focused but empty: show only the caret so typing is obvious.
            CARET.to_string()
        } else {
            style(placeholder).dim().italic().to_string()
        }
    } else if focused {
        format!("{}{CARET}", style(value).cyan().bold())
    } else {
        style(value).cyan().to_string()
    };

    format!("{block_pad}{marker} {body}")
}

/// Builds a labelled field: a dim label line followed by its input row.
fn field_lines(
    block_pad: &str,
    label: &str,
    value: &str,
    placeholder: &str,
    focused: bool,
) -> Vec<String> {
    vec![
        format!("{block_pad}{}", style(label).dim()),
        input_line(block_pad, value, placeholder, focused),
    ]
}

/// Builds the transient notice (validation error) below the form, if any.
fn notice_lines(block_pad: &str, notice: Option<&str>) -> Vec<String> {
    match notice {
        Some(notice) => vec![
            String::new(),
            format!("{block_pad}{}", style(notice).red().bold()),
        ],
        None => Vec::new(),
    }
}

/// Builds the footer help line.
fn footer_lines(width: usize) -> Vec<String> {
    let footer = "↑↓/Tab: move field / Enter: create / Esc: back";
    let pad = " ".repeat(centered_padding(width, footer.chars().count()));
    vec![String::new(), format!("{pad}{}", style(footer).dim())]
}

/// Builds the full New Project screen frame for a raw terminal size.
pub fn render_frame(
    raw_height: usize,
    raw_width: usize,
    state: &FormState,
    notice: Option<&str>,
) -> Vec<String> {
    let (height, width) = normalize_size(raw_height, raw_width);
    let block_pad = " ".repeat(centered_padding(width, BLOCK_WIDTH));

    let mut lines = header_lines(height, width);
    lines.extend(field_lines(
        &block_pad,
        "Repository URL",
        state.url(),
        "https://github.com/owner/repo.git",
        state.focus() == Field::Url,
    ));
    lines.push(String::new());
    lines.extend(field_lines(
        &block_pad,
        "Directory",
        state.directory(),
        "derived from the URL",
        state.focus() == Field::Directory,
    ));
    lines.push(String::new());
    lines.extend(field_lines(
        &block_pad,
        "Branch (optional)",
        state.branch(),
        "repository default",
        state.focus() == Field::Branch,
    ));
    lines.extend(notice_lines(&block_pad, notice));
    lines.extend(footer_lines(width));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centered_padding_centers_content() {
        assert_eq!(centered_padding(80, BLOCK_WIDTH), (80 - BLOCK_WIDTH) / 2);
    }

    #[test]
    fn centered_padding_handles_narrow_terminal() {
        assert_eq!(centered_padding(10, BLOCK_WIDTH), 0);
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
    fn header_uses_two_blank_lines_in_tall_terminals() {
        let lines = header_lines(40, 80);
        assert_eq!(lines.iter().take_while(|l| l.is_empty()).count(), 2);
        assert!(lines.iter().any(|l| l.contains("New Project")));
    }

    #[test]
    fn header_uses_one_blank_line_in_short_terminals() {
        let lines = header_lines(10, 80);
        assert_eq!(lines.iter().take_while(|l| l.is_empty()).count(), 1);
    }

    #[test]
    fn input_line_focused_and_empty_shows_only_caret() {
        let line = input_line("", "", "placeholder", true);
        assert!(line.contains(CARET));
        assert!(line.contains('>'));
        assert!(!line.contains("placeholder"));
    }

    #[test]
    fn input_line_focused_and_filled_shows_value_and_caret() {
        let line = input_line("", "repo", "placeholder", true);
        assert!(line.contains("repo"));
        assert!(line.contains(CARET));
    }

    #[test]
    fn input_line_unfocused_and_empty_shows_placeholder() {
        let line = input_line("", "", "placeholder", false);
        assert!(line.contains("placeholder"));
        assert!(!line.contains(CARET));
    }

    #[test]
    fn input_line_unfocused_and_filled_shows_value() {
        let line = input_line("", "repo", "placeholder", false);
        assert!(line.contains("repo"));
        assert!(!line.contains("placeholder"));
        assert!(!line.contains(CARET));
    }

    #[test]
    fn field_lines_render_label_and_input() {
        let lines = field_lines("", "Directory", "repo", "ph", false);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("Directory"));
        assert!(lines[1].contains("repo"));
    }

    #[test]
    fn notice_lines_empty_when_absent() {
        assert!(notice_lines("", None).is_empty());
    }

    #[test]
    fn notice_lines_render_text_when_present() {
        let lines = notice_lines("", Some("bad url"));
        assert_eq!(lines.len(), 2);
        assert!(lines[1].contains("bad url"));
    }

    #[test]
    fn footer_lines_include_help_text() {
        let lines = footer_lines(80);
        assert!(lines.iter().any(|l| l.contains("Esc")));
    }

    #[test]
    fn render_frame_combines_all_sections() {
        let mut state = FormState::new();
        for c in "https://github.com/owner/repo.git".chars() {
            state.insert_char(c);
        }
        let frame = render_frame(0, 0, &state, Some("oops"));
        let joined = frame.join("\n");
        assert!(joined.contains("New Project"));
        assert!(joined.contains("Repository URL"));
        assert!(joined.contains("repo")); // derived directory + url
        assert!(joined.contains("oops"));
        assert!(joined.contains("Esc"));
    }
}

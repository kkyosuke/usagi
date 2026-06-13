use console::style;

use crate::presentation::tui::widgets;

use super::state::{Field, FormState};

const TITLE: &str = "New Project";
const SUBTITLE: &str = "Clone a Git repository into a new workspace";

/// Fixed width of the form block; the whole block is centred in the terminal.
const BLOCK_WIDTH: usize = 52;

/// Caret shown at the end of the focused input field.
const CARET: &str = "▏";

/// Builds the centred mascot, title, and subtitle block.
///
/// Vertical placement is handled by [`render_frame`], so this adds no leading
/// padding.
fn header_lines(width: usize) -> Vec<String> {
    let mut lines = widgets::rabbit_lines(width);
    lines.push(String::new());
    lines.push(widgets::title_line(width, TITLE));
    lines.push(widgets::dim_line(width, SUBTITLE));
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

/// Builds the transient notice (validation error) below the form.
///
/// Always returns two lines — a blank separator plus the notice slot (blank
/// when absent) — so showing or clearing the error never shifts the form.
fn notice_lines(block_pad: &str, notice: Option<&str>) -> Vec<String> {
    let slot = match notice {
        Some(notice) => format!("{block_pad}{}", style(notice).red().bold()),
        None => String::new(),
    };
    vec![String::new(), slot]
}

/// Builds the footer help line.
///
/// Returns the footer text only; [`render_frame`] pins it to the bottom edge.
fn footer_lines(width: usize) -> Vec<String> {
    vec![widgets::dim_line(
        width,
        "↑↓/Tab: move field / Enter: create / Esc: back",
    )]
}

/// Builds the full New Project screen frame for a raw terminal size.
pub fn render_frame(
    raw_height: usize,
    raw_width: usize,
    state: &FormState,
    notice: Option<&str>,
) -> Vec<String> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let block_pad = " ".repeat(widgets::centered_padding(width, BLOCK_WIDTH));

    // The body (mascot, title, form fields and notice slot) is centred
    // vertically; the footer is pinned to the bottom edge of the frame.
    let mut body = header_lines(width);
    body.push(String::new());
    body.extend(field_lines(
        &block_pad,
        "Repository URL",
        state.url(),
        "https://github.com/owner/repo.git",
        state.focus() == Field::Url,
    ));
    body.push(String::new());
    body.extend(field_lines(
        &block_pad,
        "Location",
        state.location(),
        "where to create the project",
        state.focus() == Field::Location,
    ));
    body.push(String::new());
    body.extend(field_lines(
        &block_pad,
        "Directory",
        state.directory(),
        "derived from the URL",
        state.focus() == Field::Directory,
    ));
    body.push(String::new());
    body.extend(field_lines(
        &block_pad,
        "Branch (optional)",
        state.branch(),
        "repository default",
        state.focus() == Field::Branch,
    ));
    body.extend(notice_lines(&block_pad, notice));
    let footer = footer_lines(width);

    let mut lines = Vec::with_capacity(height);

    // Centre the body in the space above the footer.
    let top_padding = height.saturating_sub(body.len() + footer.len()) / 2;
    for _ in 0..top_padding {
        lines.push(String::new());
    }
    lines.extend(body);

    // Push the footer down to the bottom row of the frame.
    let bottom_padding = height.saturating_sub(lines.len() + footer.len());
    for _ in 0..bottom_padding {
        lines.push(String::new());
    }
    lines.extend(footer);

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_lines_render_mascot_title_and_subtitle() {
        let lines = header_lines(80);
        // No leading padding; the mascot block starts immediately.
        assert!(!lines[0].is_empty());
        let joined = lines.join("\n");
        assert!(joined.contains("New Project"));
        assert!(joined.contains("Clone a Git repository"));
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
    fn notice_lines_reserve_a_slot_when_absent() {
        let lines = notice_lines("", None);
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.is_empty()));
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
        assert!(joined.contains("Location"));
        assert!(joined.contains("repo")); // derived directory + url
        assert!(joined.contains("oops"));
        assert!(joined.contains("Esc"));
    }

    #[test]
    fn render_frame_centers_body_and_pins_footer() {
        let state = FormState::new();
        let height = 40;
        let frame = render_frame(height, 80, &state, None);

        assert_eq!(frame.len(), height);
        assert!(frame.last().unwrap().contains("Esc"));
        let top_padding = frame.iter().take_while(|l| l.is_empty()).count();
        assert!(top_padding > 0);
        assert!(!frame[top_padding].is_empty());
    }

    #[test]
    fn render_frame_does_not_overflow_a_short_terminal() {
        let state = FormState::new();
        let frame = render_frame(3, 80, &state, None);
        assert!(!frame[0].is_empty());
        assert!(frame.last().unwrap().contains("Esc"));
    }

    #[test]
    fn notice_slot_keeps_layout_stable_across_toggling() {
        let state = FormState::new();
        let without = render_frame(24, 80, &state, None);
        let with = render_frame(
            24,
            80,
            &state,
            Some("that does not look like a repository URL"),
        );
        assert_eq!(without.len(), with.len());
    }
}

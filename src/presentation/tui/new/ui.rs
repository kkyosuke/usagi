use console::style;

use crate::presentation::tui::widgets;

use super::state::{Field, FormState, Mode};

const TITLE: &str = "New Project";
const SUBTITLE: &str = "Clone a repository or register an existing directory";

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

/// Builds the mode selector: two tabs (`Clone` / `Existing`) with the active
/// one highlighted, and a `>` cursor plus brackets when the selector is focused.
fn mode_lines(block_pad: &str, mode: Mode, focused: bool) -> Vec<String> {
    let tab = |label: &str, active: bool| {
        if active {
            format!("[{}]", style(label).cyan().bold())
        } else {
            format!(" {} ", style(label).dim())
        }
    };
    let marker = if focused {
        style(">").red().bold().to_string()
    } else {
        " ".to_string()
    };
    let tabs = format!(
        "{}  {}",
        tab("Clone", mode == Mode::Clone),
        tab("Existing", mode == Mode::Existing),
    );
    vec![
        format!("{block_pad}{}", style("Type  (←→ to switch)").dim()),
        format!("{block_pad}{marker} {tabs}"),
    ]
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
        "←→: switch type / ↑↓/Tab: move field / Space: browse dir / Enter: create / Esc: back",
    )]
}

/// Builds the Clone-mode fields (URL, Location, Directory, Branch), each
/// separated by a blank line.
fn clone_fields(block_pad: &str, state: &FormState) -> Vec<String> {
    let mut lines = field_lines(
        block_pad,
        "Repository URL",
        state.url(),
        "https://github.com/owner/repo.git",
        state.focus() == Field::Url,
    );
    lines.push(String::new());
    lines.extend(field_lines(
        block_pad,
        "Location  (Space to browse)",
        state.location(),
        "where to create the project",
        state.focus() == Field::Location,
    ));
    lines.push(String::new());
    lines.extend(field_lines(
        block_pad,
        "Directory",
        state.directory(),
        "derived from the URL",
        state.focus() == Field::Directory,
    ));
    lines.push(String::new());
    lines.extend(field_lines(
        block_pad,
        "Branch (optional)",
        state.branch(),
        "repository default",
        state.focus() == Field::Branch,
    ));
    lines
}

/// Builds the Existing-mode fields (Directory path, Name).
fn existing_fields(block_pad: &str, state: &FormState) -> Vec<String> {
    let mut lines = field_lines(
        block_pad,
        "Directory  (Space to browse)",
        state.path(),
        "/path/to/an/existing/project",
        state.focus() == Field::Path,
    );
    lines.push(String::new());
    lines.extend(field_lines(
        block_pad,
        "Name",
        state.name(),
        "derived from the directory",
        state.focus() == Field::Name,
    ));
    lines
}

/// The reserved height of the field block, in lines: Clone is the taller mode
/// (four fields, each a label + input separated by a blank line: 4 * 2 + 3).
/// Both modes pad to this so switching modes never resizes the block.
/// [`fields_block_matches_reserved_height`] guards this against field changes.
const RESERVED_FIELDS_HEIGHT: usize = 11;

/// Builds the active mode's field block, padded to [`RESERVED_FIELDS_HEIGHT`].
///
/// Clone shows four fields and Existing only two, so without padding the body
/// would shrink and the vertically-centred layout would jump when switching
/// modes with ←→. Reserving the taller block's height in both modes keeps the
/// header, selector, and footer fixed across the switch.
fn fields_lines(block_pad: &str, state: &FormState) -> Vec<String> {
    let mut lines = match state.mode() {
        Mode::Clone => clone_fields(block_pad, state),
        Mode::Existing => existing_fields(block_pad, state),
    };
    lines.resize(lines.len().max(RESERVED_FIELDS_HEIGHT), String::new());
    lines
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

    // The body (mascot, title, mode selector, form fields and notice slot) is
    // centred vertically; the footer is pinned to the bottom edge of the frame.
    let mut body = header_lines(width);
    body.push(String::new());
    body.extend(mode_lines(
        &block_pad,
        state.mode(),
        state.focus() == Field::Mode,
    ));
    body.push(String::new());
    body.extend(fields_lines(&block_pad, state));
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
        assert!(joined.contains("register an existing directory"));
    }

    #[test]
    fn mode_lines_highlight_the_active_tab() {
        let clone = mode_lines("", Mode::Clone, false).join("\n");
        assert!(clone.contains("Clone"));
        assert!(clone.contains("Existing"));
        // Bracketing marks the active tab.
        assert!(clone.contains('['));

        let focused = mode_lines("", Mode::Existing, true).join("\n");
        assert!(focused.contains('>'));
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
        // Focus the URL field (default focus is the mode selector) before typing.
        state.focus_next();
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
        // The mode selector is shown above the fields.
        assert!(joined.contains("Clone"));
    }

    #[test]
    fn render_frame_shows_existing_mode_fields() {
        let mut state = FormState::new();
        state.toggle_mode();
        let frame = render_frame(0, 0, &state, None);
        let joined = frame.join("\n");
        // Existing mode shows a Directory path and a Name field, not URL/Branch.
        assert!(joined.contains("Directory"));
        assert!(joined.contains("Name"));
        assert!(!joined.contains("Repository URL"));
        assert!(!joined.contains("Branch"));
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
    fn fields_block_matches_reserved_height() {
        // Clone is the taller mode, so its block must be exactly the reserved
        // height, and Existing must never exceed it. This guards the constant
        // against future field additions.
        let state = FormState::new();
        assert_eq!(clone_fields("", &state).len(), RESERVED_FIELDS_HEIGHT);
        assert!(existing_fields("", &state).len() <= RESERVED_FIELDS_HEIGHT);
    }

    #[test]
    fn fields_lines_pads_existing_mode_to_the_reserved_height() {
        // Both modes occupy the reserved height, so the field block never
        // resizes when switching modes.
        let mut state = FormState::new();
        let clone = fields_lines("", &state);
        state.toggle_mode();
        let existing = fields_lines("", &state);
        assert_eq!(clone.len(), RESERVED_FIELDS_HEIGHT);
        assert_eq!(existing.len(), RESERVED_FIELDS_HEIGHT);
    }

    #[test]
    fn switching_modes_does_not_shift_the_layout() {
        // The whole frame keeps its size and the header/selector stay put when
        // toggling between Clone and Existing — no layout shift (CLS).
        let mut state = FormState::new();
        let clone = render_frame(24, 80, &state, None);
        state.toggle_mode();
        let existing = render_frame(24, 80, &state, None);
        assert_eq!(clone.len(), existing.len());
        // The mascot/title/selector rows above the fields are byte-for-byte
        // identical, so nothing above the form moves.
        let selector_row = clone.iter().position(|l| l.contains("Type")).unwrap();
        assert_eq!(clone[..=selector_row], existing[..=selector_row]);
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

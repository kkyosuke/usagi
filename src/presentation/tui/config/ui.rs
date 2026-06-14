use console::style;

use crate::presentation::tui::widgets;

use super::state::{Config, Field, LocalField};

/// The label of the Save button row.
const SAVE_LABEL: &str = "[ Save ]";

/// Fixed width of the settings block; the whole block is centred in the terminal.
const BLOCK_WIDTH: usize = 52;

/// Builds the centred mascot, title, and subtitle block.
///
/// The title and subtitle reflect the screen's scope (global vs. workspace), so
/// they are passed in. Vertical placement is handled by [`render_frame`], so
/// this adds no leading padding.
fn header_lines(width: usize, title: &str, subtitle: &str) -> Vec<String> {
    let mut lines = widgets::rabbit_lines(width);
    lines.push(String::new());
    lines.push(widgets::title_line(width, title));
    lines.push(widgets::dim_line(width, subtitle));
    lines
}

/// The width of the label column: the widest field label across both the
/// global and project-local fields, so the value column aligns whether or not
/// the local override rows are shown.
fn label_width() -> usize {
    let global = Field::ALL.iter().map(|f| f.label().chars().count());
    let local = LocalField::ALL.iter().map(|f| f.label().chars().count());
    global.chain(local).max().unwrap_or(0)
}

/// Builds one setting row. Two single-column gutters sit left of the label: the
/// `>` cursor for the selected row, then a `●` flag for a field carrying an
/// unsaved edit. The label follows in a fixed-width column, then the already
/// styled `< value >` chooser.
///
/// `value` arrives pre-styled from [`widgets::chooser`]; keeping the cursor and
/// change flag in their own gutters keeps the label and value columns aligned.
fn setting_row(
    block_pad: &str,
    label: &str,
    label_width: usize,
    value: &str,
    selected: bool,
    changed: bool,
) -> String {
    let cursor = if selected {
        style(">").red().bold().to_string()
    } else {
        " ".to_string()
    };

    // A dot to the left of the label flags an edit that has not been saved yet.
    let mark = if changed {
        style("●").yellow().bold().to_string()
    } else {
        " ".to_string()
    };

    let padded = format!("{label:<label_width$}");
    let label = if selected {
        style(padded).cyan().bold().to_string()
    } else {
        style(padded).cyan().to_string()
    };

    format!("{block_pad}{cursor} {mark} {label}  {value}")
}

/// Builds the settings list: one row per editable field (global, then any
/// project-local overrides), each rendered as a `< value >` chooser that lights
/// up when focused and turns yellow when edited.
fn settings_lines(block_pad: &str, config: &Config) -> Vec<String> {
    let label_width = label_width();

    config
        .rows()
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let selected = i == config.selected_index();
            let value = widgets::chooser(&row.value, selected, row.changed);
            setting_row(
                block_pad,
                row.label,
                label_width,
                &value,
                selected,
                row.changed,
            )
        })
        .collect()
}

/// Builds the Save button row, sat in the value column below the fields.
///
/// The button is enabled (green) only when there are unsaved changes; with
/// nothing to save it is dimmed, so its state mirrors whether pressing it would
/// do anything. The `>` cursor marks it when focused, like any other row.
fn save_button_line(block_pad: &str, dirty: bool, selected: bool) -> String {
    let marker = if selected {
        style(">").red().bold().to_string()
    } else {
        " ".to_string()
    };

    let button = if dirty {
        style(SAVE_LABEL).green().bold().to_string()
    } else {
        style(SAVE_LABEL).dim().to_string()
    };

    // Align the button under the value column: the cursor gutter, the (empty)
    // change-flag gutter, then the label-width gutter, matching `setting_row`.
    let gutter = " ".repeat(label_width());
    format!("{block_pad}{marker}   {gutter}  {button}")
}

/// Builds the transient notice line below the settings.
///
/// Always returns two lines — a blank separator plus the notice slot (blank
/// when absent) — so showing or clearing a notice never shifts the layout.
fn notice_lines(block_pad: &str, notice: Option<&str>) -> Vec<String> {
    let slot = match notice {
        Some(notice) => format!("{block_pad}{}", style(notice).yellow()),
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
        "↑↓: move · ←→: change · Enter: save · Esc: back",
    )]
}

/// Builds the full configuration frame for a raw terminal size.
pub fn render_frame(
    raw_height: usize,
    raw_width: usize,
    config: &Config,
    notice: Option<&str>,
) -> Vec<String> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let block_pad = " ".repeat(widgets::centered_padding(width, BLOCK_WIDTH));

    // The body (mascot, title, settings and notice slot) is centred vertically;
    // the footer is pinned to the bottom edge of the frame.
    let mut body = header_lines(width, config.title(), config.subtitle());
    body.push(String::new());
    body.extend(settings_lines(&block_pad, config));
    // A blank line sets the Save button apart from the fields above it.
    body.push(String::new());
    body.push(save_button_line(
        &block_pad,
        config.is_dirty(),
        config.is_save_selected(),
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
    use crate::domain::settings::{Settings, Theme};

    /// A row carries the cursor when its first non-space glyph is `>`. Chevrons
    /// from the chooser also contain `>`, so position — not mere presence — is
    /// what distinguishes the selected row.
    fn has_cursor(line: &str) -> bool {
        console::strip_ansi_codes(line)
            .trim_start()
            .starts_with('>')
    }

    fn sample_config() -> Config {
        Config::new(
            Settings {
                theme: Theme::Dark,
                default_workspace: Some("alpha".to_string()),
                ..Default::default()
            },
            vec!["alpha".to_string()],
        )
    }

    #[test]
    fn header_lines_render_mascot_title_and_subtitle() {
        let lines = header_lines(80, "Config", "Adjust your global preferences");
        assert!(!lines[0].is_empty());
        let joined = lines.join("\n");
        assert!(joined.contains("Config"));
        assert!(joined.contains("Adjust your global preferences"));
    }

    #[test]
    fn setting_row_marks_only_the_selected_entry() {
        let selected = setting_row("", "Theme", 17, "Dark", true, false);
        assert!(selected.contains('>'));
        assert!(selected.contains("Theme"));
        assert!(selected.contains("Dark"));
        let unselected = setting_row("", "Theme", 17, "Dark", false, false);
        assert!(!unselected.contains('>'));
    }

    #[test]
    fn setting_row_flags_changed_fields_with_a_dot() {
        let changed = setting_row("", "Theme", 17, "Dark", false, true);
        assert!(changed.contains('●'));
        let unchanged = setting_row("", "Theme", 17, "Dark", false, false);
        assert!(!unchanged.contains('●'));
    }

    #[test]
    fn settings_lines_render_one_row_per_field() {
        let config = sample_config();
        let lines = settings_lines("", &config);
        assert_eq!(lines.len(), Field::ALL.len());
        assert!(lines[0].contains("Theme"));
        assert!(lines[0].contains("Dark"));
        assert!(lines[1].contains("Default Workspace"));
        assert!(lines[1].contains("alpha"));
        assert!(lines[2].contains("Notifications"));
        assert!(lines[2].contains("On"));
        assert!(lines[3].contains("Agent CLI"));
        // Each field shows its single current value via the chooser.
        assert!(lines[3].contains("Claude"));
        // Every field is a chooser, so chevrons appear on all rows...
        assert!(lines.iter().all(|l| l.contains('<') && l.contains('>')));
        // ...but only the focused (first) row carries the cursor.
        assert!(has_cursor(&lines[0]));
        assert_eq!(lines.iter().filter(|l| has_cursor(l)).count(), 1);
        // Nothing is edited in a fresh config, so no row is flagged changed.
        assert!(lines.iter().all(|l| !l.contains('●')));
    }

    #[test]
    fn settings_lines_flag_an_edited_field() {
        let mut config = sample_config();
        config.cycle_selected(true); // edit the focused Theme field
        let lines = settings_lines("", &config);
        assert!(lines[0].contains('●'));
        assert!(lines[1..].iter().all(|l| !l.contains('●')));
    }

    #[test]
    fn notice_lines_reserve_a_slot_when_absent() {
        let lines = notice_lines("", None);
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.is_empty()));
    }

    #[test]
    fn notice_lines_render_text_when_present() {
        let lines = notice_lines("", Some("Saved 🐰"));
        assert_eq!(lines.len(), 2);
        assert!(lines[1].contains("Saved"));
    }

    #[test]
    fn footer_lines_include_help_text() {
        let lines = footer_lines(80);
        assert!(lines.iter().any(|l| l.contains("Esc")));
        assert!(lines.iter().any(|l| l.contains("save")));
    }

    #[test]
    fn save_button_line_renders_the_button_and_cursor() {
        // Dirty + selected: shows the label and carries the cursor.
        let selected = save_button_line("", true, true);
        assert!(selected.contains("Save"));
        assert!(selected.contains('>'));
        // Clean + unselected: still shows the label, no cursor.
        let idle = save_button_line("", false, false);
        assert!(idle.contains("Save"));
        assert!(!idle.contains('>'));
    }

    #[test]
    fn render_frame_combines_all_sections() {
        let config = sample_config();
        let frame = render_frame(0, 0, &config, Some("Saved 🐰"));
        let joined = frame.join("\n");
        assert!(joined.contains("Config"));
        assert!(joined.contains("Theme"));
        assert!(joined.contains("Dark"));
        // The Save button is part of every frame.
        assert!(joined.contains("Save"));
        assert!(joined.contains("Saved"));
        assert!(joined.contains("Esc"));
    }

    #[test]
    fn render_frame_marks_the_save_button_when_focused() {
        let mut config = sample_config();
        // Move the cursor down onto the Save button (past all the fields).
        for _ in 0..Field::ALL.len() {
            config.move_down();
        }
        assert!(config.is_save_selected());
        let frame = render_frame(24, 80, &config, None);
        // No field row carries the cursor; the Save row does.
        let cursor_rows: Vec<&String> = frame.iter().filter(|l| has_cursor(l)).collect();
        assert_eq!(cursor_rows.len(), 1);
        assert!(cursor_rows[0].contains("Save"));
    }

    #[test]
    fn render_frame_centers_body_and_pins_footer() {
        let config = sample_config();
        let height = 40;
        let frame = render_frame(height, 80, &config, None);

        assert_eq!(frame.len(), height);
        assert!(frame.last().unwrap().contains("Esc"));
        let top_padding = frame.iter().take_while(|l| l.is_empty()).count();
        assert!(top_padding > 0);
        assert!(!frame[top_padding].is_empty());
    }

    #[test]
    fn render_frame_does_not_overflow_a_short_terminal() {
        let config = sample_config();
        let frame = render_frame(3, 80, &config, None);
        assert!(!frame[0].is_empty());
        assert!(frame.last().unwrap().contains("Esc"));
    }

    #[test]
    fn notice_slot_keeps_layout_stable_across_toggling() {
        let config = sample_config();
        let without = render_frame(24, 80, &config, None);
        let with = render_frame(24, 80, &config, Some("Saved 🐰"));
        assert_eq!(without.len(), with.len());
    }
}

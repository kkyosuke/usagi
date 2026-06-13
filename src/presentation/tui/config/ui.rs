use console::style;

use crate::presentation::tui::widgets;

use super::state::{Config, Field};

const TITLE: &str = "Config";
const SUBTITLE: &str = "Adjust your preferences";

/// Fixed width of the settings block; the whole block is centred in the terminal.
const BLOCK_WIDTH: usize = 52;

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

/// Builds one setting row: a `>` cursor for the selected entry, the label in a
/// fixed-width column, and the current value dimmed beside it.
fn setting_row(
    block_pad: &str,
    label: &str,
    label_width: usize,
    value: &str,
    selected: bool,
) -> String {
    let marker = if selected {
        style(">").red().bold().to_string()
    } else {
        " ".to_string()
    };

    let padded = format!("{label:<label_width$}");
    let label = if selected {
        style(padded).cyan().bold().to_string()
    } else {
        style(padded).cyan().to_string()
    };

    let value = style(value).dim().to_string();

    format!("{block_pad}{marker} {label}  {value}")
}

/// Builds the settings list: one row per editable field.
fn settings_lines(block_pad: &str, config: &Config) -> Vec<String> {
    let label_width = Field::ALL
        .iter()
        .map(|f| f.label().chars().count())
        .max()
        .unwrap_or(0);

    Field::ALL
        .iter()
        .enumerate()
        .map(|(i, &field)| {
            setting_row(
                block_pad,
                field.label(),
                label_width,
                &config.value_of(field),
                i == config.selected_index(),
            )
        })
        .collect()
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
        "↑↓: move / ←→/Enter: change / Esc: back",
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
    let mut body = header_lines(width);
    body.push(String::new());
    body.extend(settings_lines(&block_pad, config));
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
        let lines = header_lines(80);
        assert!(!lines[0].is_empty());
        let joined = lines.join("\n");
        assert!(joined.contains("Config"));
        assert!(joined.contains("Adjust your preferences"));
    }

    #[test]
    fn setting_row_marks_only_the_selected_entry() {
        let selected = setting_row("", "Theme", 17, "Dark", true);
        assert!(selected.contains('>'));
        assert!(selected.contains("Theme"));
        assert!(selected.contains("Dark"));
        let unselected = setting_row("", "Theme", 17, "Dark", false);
        assert!(!unselected.contains('>'));
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
        // Only the first (selected) row carries the cursor.
        assert_eq!(lines.iter().filter(|l| l.contains('>')).count(), 1);
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
    }

    #[test]
    fn render_frame_combines_all_sections() {
        let config = sample_config();
        let frame = render_frame(0, 0, &config, Some("Saved 🐰"));
        let joined = frame.join("\n");
        assert!(joined.contains("Config"));
        assert!(joined.contains("Theme"));
        assert!(joined.contains("Dark"));
        assert!(joined.contains("Saved"));
        assert!(joined.contains("Esc"));
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

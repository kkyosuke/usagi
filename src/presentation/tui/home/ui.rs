use console::style;

/// A single entry in the startup-screen menu.
pub struct MenuItem {
    pub label: &'static str,
    pub key: char,
}

const RABBIT_LINES: [&str; 3] = ["  (\\(\\ ", " (='-') ", " o(_(\")(\")"];

const TITLE: &str = "USAGI";

/// Left padding that centres content of the given width in the terminal.
fn centered_padding(term_width: usize, content_width: usize) -> usize {
    term_width.saturating_sub(content_width) / 2
}

/// Normalises a raw terminal size, substituting fallbacks for the zeroes that
/// non-interactive environments report.
pub fn normalize_size(height: usize, width: usize) -> (usize, usize) {
    let height = if height == 0 { 24 } else { height };
    let width = if width == 0 { 80 } else { width };
    (height, width)
}

/// Builds the ASCII-art mascot lines, centred for the given terminal size.
fn rabbit_lines(height: usize, width: usize) -> Vec<String> {
    let mut lines = Vec::new();

    let total_height = RABBIT_LINES.len() + 3;
    let top_padding = if height > total_height + 10 {
        (height - total_height) / 4
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
    let rabbit_padding = " ".repeat(centered_padding(width, rabbit_width));
    for line in RABBIT_LINES {
        lines.push(
            style(format!("{rabbit_padding}{line}"))
                .magenta()
                .bold()
                .to_string(),
        );
    }

    lines.push(String::new());
    let title_padding = " ".repeat(centered_padding(width, TITLE.chars().count()));
    lines.push(
        style(format!("{title_padding}{TITLE}"))
            .green()
            .bold()
            .to_string(),
    );

    lines
}

/// Builds the menu lines, highlighting the selected entry.
fn menu_lines(width: usize, items: &[MenuItem], selected_index: usize) -> Vec<String> {
    // "> Label..... key" — cursor + 10-char label + right-aligned key.
    let menu_width = 18;
    let left_padding = " ".repeat(centered_padding(width, menu_width));

    let mut lines = vec![String::new()];
    for (i, item) in items.iter().enumerate() {
        let is_selected = i == selected_index;

        let cursor = if is_selected {
            style(">").red().bold().to_string()
        } else {
            " ".to_string()
        };
        let label = if is_selected {
            style(format!("{:<10}", item.label))
                .cyan()
                .bold()
                .to_string()
        } else {
            format!("{:<10}", item.label)
        };
        let key = if is_selected {
            style(format!("{:>5}", item.key)).yellow().to_string()
        } else {
            format!("{:>5}", item.key)
        };

        lines.push(format!("{left_padding}{cursor} {label} {key}"));
        lines.push(String::new());
    }
    lines
}

/// Builds the transient notice line under the menu, if any.
fn notice_lines(width: usize, notice: Option<&str>) -> Vec<String> {
    let Some(notice) = notice else {
        return Vec::new();
    };
    let padding = " ".repeat(centered_padding(width, notice.chars().count()));
    vec![format!("{padding}{}", style(notice).yellow())]
}

/// Builds the status footer shown at the bottom of the startup screen.
fn footer_lines(width: usize) -> Vec<String> {
    let footer = format!(
        " v{} | ↑↓: move / Enter: select / q: quit",
        env!("CARGO_PKG_VERSION")
    );
    let padding = " ".repeat(centered_padding(width, footer.chars().count()));
    vec![String::new(), format!("{padding}{}", style(footer).dim())]
}

/// Builds the full startup-screen frame for a raw terminal size.
pub fn render_frame(
    raw_height: usize,
    raw_width: usize,
    items: &[MenuItem],
    selected_index: usize,
    notice: Option<&str>,
) -> Vec<String> {
    let (height, width) = normalize_size(raw_height, raw_width);
    let mut lines = rabbit_lines(height, width);
    lines.extend(menu_lines(width, items, selected_index));
    lines.extend(notice_lines(width, notice));
    lines.extend(footer_lines(width));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_items() -> Vec<MenuItem> {
        vec![
            MenuItem {
                label: "Open",
                key: 'o',
            },
            MenuItem {
                label: "Quit",
                key: 'q',
            },
        ]
    }

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
    fn rabbit_lines_uses_quarter_padding_in_tall_terminals() {
        // total_height = 6, threshold = 16, so a height of 40 takes the tall branch.
        let lines = rabbit_lines(40, 80);
        // (40 - 6) / 4 == 8 leading blank lines.
        assert_eq!(lines.iter().take_while(|l| l.is_empty()).count(), 8);
        assert!(lines.iter().any(|l| l.contains("USAGI")));
    }

    #[test]
    fn rabbit_lines_uses_single_padding_in_short_terminals() {
        let lines = rabbit_lines(10, 80);
        assert_eq!(lines.iter().take_while(|l| l.is_empty()).count(), 1);
    }

    #[test]
    fn menu_lines_marks_only_the_selected_entry() {
        let items = sample_items();
        let lines = menu_lines(80, &items, 0);
        assert!(lines.iter().any(|l| l.contains("Open")));
        assert!(lines.iter().any(|l| l.contains("Quit")));
        // The selected cursor ">" appears exactly once.
        assert_eq!(lines.iter().filter(|l| l.contains('>')).count(), 1);
    }

    #[test]
    fn notice_lines_empty_when_absent() {
        assert!(notice_lines(80, None).is_empty());
    }

    #[test]
    fn notice_lines_renders_text_when_present() {
        let lines = notice_lines(80, Some("hello"));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("hello"));
    }

    #[test]
    fn footer_lines_include_version() {
        let lines = footer_lines(80);
        assert!(lines.iter().any(|l| l.contains(env!("CARGO_PKG_VERSION"))));
    }

    #[test]
    fn render_frame_combines_all_sections() {
        let items = sample_items();
        let frame = render_frame(0, 0, &items, 1, Some("coming soon"));
        let joined = frame.join("\n");
        assert!(joined.contains("USAGI"));
        assert!(joined.contains("Open"));
        assert!(joined.contains("coming soon"));
        assert!(joined.contains(env!("CARGO_PKG_VERSION")));
    }
}

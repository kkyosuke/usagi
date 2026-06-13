use console::style;

use crate::presentation::tui::widgets;

/// A single entry in the welcome-screen menu.
pub struct MenuItem {
    pub label: &'static str,
    pub key: char,
}

const TITLE: &str = "USAGI";

/// Builds the centred mascot and title block.
///
/// Vertical placement is handled by [`render_frame`], which centres the whole
/// body in the terminal, so this returns no leading padding.
fn header_lines(width: usize) -> Vec<String> {
    let mut lines = widgets::rabbit_lines(width);
    lines.push(String::new());
    lines.push(widgets::title_line(width, TITLE));
    lines
}

/// Builds the menu lines, highlighting the selected entry.
fn menu_lines(width: usize, items: &[MenuItem], selected_index: usize) -> Vec<String> {
    // "> Label..... key" — cursor + 10-char label + right-aligned key.
    let menu_width = 18;
    let left_padding = " ".repeat(widgets::centered_padding(width, menu_width));

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

/// Builds the transient notice line under the menu.
///
/// Always returns exactly one line — a blank placeholder when there is no
/// notice — so showing or clearing a notice never shifts the surrounding
/// layout.
fn notice_lines(width: usize, notice: Option<&str>) -> Vec<String> {
    let Some(notice) = notice else {
        return vec![String::new()];
    };
    let padding = " ".repeat(widgets::centered_padding(width, notice.chars().count()));
    vec![format!("{padding}{}", style(notice).yellow())]
}

/// Builds the status footer shown at the bottom of the welcome screen.
///
/// Returns the footer text only; [`render_frame`] pins it to the bottom edge.
fn footer_lines(width: usize) -> Vec<String> {
    let footer = format!(
        " v{} | ↑↓: move / Enter: select / q: quit",
        env!("CARGO_PKG_VERSION")
    );
    vec![widgets::dim_line(width, &footer)]
}

/// Builds the full welcome-screen frame for a raw terminal size.
pub fn render_frame(
    raw_height: usize,
    raw_width: usize,
    items: &[MenuItem],
    selected_index: usize,
    notice: Option<&str>,
) -> Vec<String> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);

    // The body (mascot, title, menu and notice slot) is centred vertically;
    // the footer is pinned to the bottom edge of the frame.
    let mut body = header_lines(width);
    body.extend(menu_lines(width, items, selected_index));
    body.extend(notice_lines(width, notice));
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
    fn header_lines_have_no_leading_padding() {
        // Vertical placement is handled by render_frame, so the mascot block
        // itself starts immediately with the first rabbit line.
        let lines = header_lines(80);
        assert!(!lines[0].is_empty());
        assert!(lines.iter().any(|l| l.contains("USAGI")));
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
    fn notice_lines_reserve_a_blank_slot_when_absent() {
        // A reserved blank line keeps the layout from shifting when a notice
        // appears or is cleared.
        let lines = notice_lines(80, None);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].is_empty());
    }

    #[test]
    fn notice_slot_keeps_layout_stable_across_toggling() {
        let items = sample_items();
        let without = render_frame(24, 80, &items, 0, None);
        let with = render_frame(24, 80, &items, 0, Some("Config is coming soon 🐰"));
        // The frame height is identical whether or not a notice is shown, so
        // the menu and footer never move.
        assert_eq!(without.len(), with.len());
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

    #[test]
    fn render_frame_centers_body_and_pins_footer() {
        let items = sample_items();
        let height = 40;
        let frame = render_frame(height, 80, &items, 0, None);

        // The frame fills exactly the terminal height...
        assert_eq!(frame.len(), height);
        // ...with the footer on the very last line...
        assert!(frame.last().unwrap().contains(env!("CARGO_PKG_VERSION")));
        // ...and leading blank lines that vertically centre the body.
        let top_padding = frame.iter().take_while(|l| l.is_empty()).count();
        assert!(top_padding > 0);
        assert!(!frame[top_padding].is_empty());
        assert!(frame.iter().any(|l| l.contains("USAGI")));
    }

    #[test]
    fn render_frame_does_not_overflow_a_short_terminal() {
        let items = sample_items();
        // A terminal too short for the body: no centring padding is added, and
        // the body is never truncated.
        let frame = render_frame(3, 80, &items, 0, None);
        assert!(!frame[0].is_empty());
        assert!(frame.iter().any(|l| l.contains("USAGI")));
        assert!(frame.last().unwrap().contains(env!("CARGO_PKG_VERSION")));
    }
}

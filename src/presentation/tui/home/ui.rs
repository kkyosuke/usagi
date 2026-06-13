use console::{style, Term};

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

/// Terminal size with fallbacks for environments that report zero.
fn term_size(term: &Term) -> (usize, usize) {
    let (height, width) = term.size();
    let height = if height == 0 { 24 } else { height as usize };
    let width = if width == 0 { 80 } else { width as usize };
    (height, width)
}

/// Renders the usagi ASCII-art mascot centred in the terminal.
pub fn show_rabbit(term: &Term) {
    let (height, width) = term_size(term);

    let total_height = RABBIT_LINES.len() + 3;
    let top_padding = if height > total_height + 10 {
        (height - total_height) / 4
    } else {
        1
    };
    for _ in 0..top_padding {
        let _ = term.write_line("");
    }

    let rabbit_width = RABBIT_LINES
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0);
    let rabbit_padding = " ".repeat(centered_padding(width, rabbit_width));
    for line in RABBIT_LINES {
        let _ = term.write_line(
            &style(format!("{rabbit_padding}{line}"))
                .magenta()
                .bold()
                .to_string(),
        );
    }

    let _ = term.write_line("");
    let title_padding = " ".repeat(centered_padding(width, TITLE.chars().count()));
    let _ = term.write_line(
        &style(format!("{title_padding}{TITLE}"))
            .green()
            .bold()
            .to_string(),
    );
}

/// Renders the menu items, highlighting the selected entry.
pub fn render_side_menu(term: &Term, items: &[MenuItem], selected_index: usize) {
    let (_, width) = term_size(term);

    // "> Label..... key" — cursor + 10-char label + right-aligned key.
    let menu_width = 18;
    let left_padding = " ".repeat(centered_padding(width, menu_width));

    let _ = term.write_line("");
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

        let _ = term.write_line(&format!("{left_padding}{cursor} {label} {key}"));
        let _ = term.write_line("");
    }
}

/// Renders a transient notice line under the menu, if any.
pub fn render_notice(term: &Term, notice: Option<&str>) {
    let Some(notice) = notice else {
        return;
    };
    let (_, width) = term_size(term);
    let padding = " ".repeat(centered_padding(width, notice.chars().count()));
    let _ = term.write_line(&format!("{padding}{}", style(notice).yellow()));
}

/// Renders the status footer at the bottom of the startup screen.
pub fn render_footer(term: &Term) {
    let footer = format!(
        " v{} | ↑↓: move / Enter: select / q: quit",
        env!("CARGO_PKG_VERSION")
    );
    let (_, width) = term_size(term);
    let padding = " ".repeat(centered_padding(width, footer.chars().count()));

    let _ = term.write_line("");
    let _ = term.write_line(&format!("{padding}{}", style(footer).dim()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_centered_padding_centers_content() {
        assert_eq!(centered_padding(80, 10), 35);
        assert_eq!(centered_padding(81, 10), 35);
    }

    #[test]
    fn test_centered_padding_handles_narrow_terminal() {
        assert_eq!(centered_padding(5, 10), 0);
    }
}

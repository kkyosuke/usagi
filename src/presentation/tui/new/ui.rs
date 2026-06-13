use console::{style, Term};

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

/// Terminal size with fallbacks for environments that report zero.
fn term_size(term: &Term) -> (usize, usize) {
    let (height, width) = term.size();
    let height = if height == 0 { 24 } else { height as usize };
    let width = if width == 0 { 80 } else { width as usize };
    (height, width)
}

/// Renders the whole New Project screen.
pub fn render(term: &Term, state: &FormState, notice: Option<&str>) {
    let (height, width) = term_size(term);
    let block_pad = " ".repeat(centered_padding(width, BLOCK_WIDTH));

    render_header(term, width, height);

    render_label(term, &block_pad, "Repository URL");
    render_input(
        term,
        &block_pad,
        state.url(),
        "https://github.com/owner/repo.git",
        state.focus() == Field::Url,
    );
    blank(term);

    render_label(term, &block_pad, "Directory");
    render_input(
        term,
        &block_pad,
        state.directory(),
        "derived from the URL",
        state.focus() == Field::Directory,
    );
    blank(term);

    render_label(term, &block_pad, "Branch (optional)");
    render_input(
        term,
        &block_pad,
        state.branch(),
        "repository default",
        state.focus() == Field::Branch,
    );

    render_notice(term, &block_pad, notice);
    render_footer(term, width);
}

fn render_header(term: &Term, width: usize, height: usize) {
    let top_padding = if height > RABBIT_LINES.len() + 14 {
        2
    } else {
        1
    };
    for _ in 0..top_padding {
        blank(term);
    }

    let rabbit_width = RABBIT_LINES
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0);
    let rabbit_pad = " ".repeat(centered_padding(width, rabbit_width));
    for line in RABBIT_LINES {
        let _ = term.write_line(
            &style(format!("{rabbit_pad}{line}"))
                .magenta()
                .bold()
                .to_string(),
        );
    }

    blank(term);
    let title_pad = " ".repeat(centered_padding(width, TITLE.chars().count()));
    let _ = term.write_line(
        &style(format!("{title_pad}{TITLE}"))
            .green()
            .bold()
            .to_string(),
    );
    let subtitle_pad = " ".repeat(centered_padding(width, SUBTITLE.chars().count()));
    let _ = term.write_line(&format!("{subtitle_pad}{}", style(SUBTITLE).dim()));
    blank(term);
}

/// Renders a field label, left-aligned within the centred block.
fn render_label(term: &Term, block_pad: &str, label: &str) {
    let _ = term.write_line(&format!("{block_pad}{}", style(label).dim()));
}

/// Renders one input row: a `>` cursor for the focused field, the value (or a
/// dim placeholder when empty), and a caret on the focused field.
fn render_input(term: &Term, block_pad: &str, value: &str, placeholder: &str, focused: bool) {
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

    let _ = term.write_line(&format!("{block_pad}{marker} {body}"));
}

/// Renders a transient notice (validation error) below the form.
fn render_notice(term: &Term, block_pad: &str, notice: Option<&str>) {
    blank(term);
    let Some(notice) = notice else {
        return;
    };
    let _ = term.write_line(&format!("{block_pad}{}", style(notice).red().bold()));
}

fn render_footer(term: &Term, width: usize) {
    let footer = "↑↓/Tab: move field / Enter: create / Esc: back";
    let pad = " ".repeat(centered_padding(width, footer.chars().count()));
    blank(term);
    let _ = term.write_line(&format!("{pad}{}", style(footer).dim()));
}

fn blank(term: &Term) {
    let _ = term.write_line("");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centered_padding_centers_block() {
        assert_eq!(centered_padding(80, BLOCK_WIDTH), (80 - BLOCK_WIDTH) / 2);
    }

    #[test]
    fn centered_padding_handles_narrow_terminal() {
        assert_eq!(centered_padding(10, BLOCK_WIDTH), 0);
    }
}

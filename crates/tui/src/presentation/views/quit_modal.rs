//! Detach / quit confirmation overlay.
//!
//! A stateless renderer for the controller's `Overlay::QuitConfirmation`. The
//! reducer owns the decision (`y`/Enter detaches, `n`/Esc stays) and the Yes/No
//! focus, so this surface only projects that focus onto the shared confirmation
//! renderer the shell composites over a `render_home` frame.

use crate::presentation::theme::{Color, Role, Style};
use crate::presentation::widgets::modal::{self, ConfirmationModal, ConfirmationView};

const INNER_WIDTH: usize = 48;

/// Render the quit confirmation over an existing Home frame without replacing its
/// background. `confirm_selected` mirrors the reducer's Yes/No focus so the
/// shared `[ yes ] [ no ]` buttons highlight the choice the user will commit.
#[must_use]
pub fn render_over(
    height: usize,
    width: usize,
    base: &[String],
    confirm_selected: bool,
) -> Vec<String> {
    let title = Style::new().fg(Color::White).bold().paint("Quit");
    let heading = Style::new()
        .fg(Color::White)
        .bold()
        .paint("Detach from this workspace?");
    modal::render_confirmation_over(
        height,
        width,
        base,
        ConfirmationModal::from_confirm_selected(confirm_selected),
        ConfirmationView {
            title: &title,
            inner_width: INNER_WIDTH,
            heading,
            message: "Sessions keep running in the background.",
            confirm_role: Role::Danger,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::render_over;
    use crate::presentation::widgets::display_width;

    #[test]
    fn draws_the_detach_prompt_with_shared_yes_no_buttons() {
        let base = vec!["home".to_owned(); 20];
        let frame = render_over(20, 60, &base, true);
        let text = frame.join("\n");
        assert!(text.contains("Quit"));
        assert!(text.contains("Detach from this workspace?"));
        assert!(text.contains("Sessions keep running in the background."));
        // Shared confirmation buttons and shortcut line replace the old y/n copy.
        assert!(text.contains("[ yes ]"));
        assert!(text.contains("[ no  ]"));
        assert!(text.contains("Enter/y: yes"));
        assert!(text.contains("Esc/n: no"));
        assert!(text.contains("←→/Tab: choose"));
        assert!(frame.iter().all(|line| display_width(line) <= 60));
    }

    #[test]
    fn reflects_the_reducer_yes_no_focus() {
        let base = vec!["home".to_owned(); 20];
        let yes = render_over(20, 60, &base, true).join("\n");
        let no = render_over(20, 60, &base, false).join("\n");
        // Yes focused: the affirmative button carries the bold danger SGR; No
        // focused: the negative button carries the bold warning SGR instead.
        assert!(yes.contains("\u{1b}[1;31m[ yes ]"));
        assert!(no.contains("\u{1b}[1;33m[ no  ]"));
        assert_ne!(yes, no);
    }
}

//! Workspace exit prompt.
//!
//! A stateless renderer for the controller's `Overlay::QuitConfirmation`. The
//! reducer owns the decision and the focused button ([`ExitChoice`]), so this
//! surface only projects that focus onto the shared choice renderer the shell
//! composites over a `render_home` frame.
//!
//! The prompt offers three answers because leaving a workspace and ending the
//! process are different intents: `welcome` returns to the switcher with the
//! process alive, `quit` ends the TUI, `stay` cancels. Each has its own letter
//! and its own button, so neither can be reached by mistyping the other (#556).

use crate::presentation::theme::{Color, Role, Style};
use crate::presentation::widgets::modal::{self, ChoiceView};
use crate::usecase::application::controller::ExitChoice;

const INNER_WIDTH: usize = 48;

/// The button row, built from [`ExitChoice::ORDER`] so display order is decided
/// once — by the reducer that also owns [`ExitChoice::index`] — and the focus can
/// never land on a different button than the one drawn.
///
/// Quitting keeps the Danger role and staying the Warning cancel, as in every
/// Yes/No confirmation; leaving is neither destructive nor a cancel, so it takes
/// the Accent role and is visibly a third thing rather than a shade of quit.
fn choices() -> [(&'static str, Role); 3] {
    ExitChoice::ORDER.map(|choice| match choice {
        ExitChoice::Welcome => ("welcome", Role::Accent),
        ExitChoice::Quit => ("quit", Role::Danger),
        ExitChoice::Stay => ("stay", Role::Warning),
    })
}

const HINTS: [&str; 2] = [
    "w: welcome   q: quit   Esc/n: stay",
    "Enter: choose   ←→/Tab: move",
];

/// Render the exit prompt over an existing Home frame without replacing its
/// background. `choice` mirrors the reducer's focus so the shared button row
/// highlights the answer Enter will commit.
#[must_use]
pub fn render_over(
    height: usize,
    width: usize,
    base: &[String],
    choice: ExitChoice,
) -> Vec<String> {
    let title = Style::new().fg(Color::White).bold().paint("Exit");
    let heading = Style::new()
        .fg(Color::White)
        .bold()
        .paint("Leave this workspace?");
    modal::render_choice_over(
        height,
        width,
        base,
        choice.index(),
        ChoiceView {
            title: &title,
            inner_width: INNER_WIDTH,
            heading,
            message: "welcome keeps usagi running; quit ends it.",
            choices: &choices(),
            hints: &HINTS,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::render_over;
    use crate::presentation::widgets::display_width;
    use crate::usecase::application::controller::ExitChoice;

    #[test]
    fn draws_the_three_exit_answers_with_their_own_keys() {
        let base = vec!["home".to_owned(); 20];
        let frame = render_over(20, 60, &base, ExitChoice::Quit);
        let text = frame.join("\n");
        assert!(text.contains("Exit"));
        assert!(text.contains("Leave this workspace?"));
        assert!(text.contains("welcome keeps usagi running; quit ends it."));
        // Leaving and quitting are separate buttons with separate letters, so the
        // UI never conflates them.
        assert!(text.contains("[ welcome ]"));
        assert!(text.contains("[ quit    ]"));
        assert!(text.contains("[ stay    ]"));
        assert!(text.contains("w: welcome"));
        assert!(text.contains("q: quit"));
        assert!(text.contains("Esc/n: stay"));
        assert!(text.contains("Enter: choose"));
        assert!(text.contains("←→/Tab: move"));
        assert!(frame.iter().all(|line| display_width(line) <= 60));
    }

    #[test]
    fn reflects_the_reducer_focus_on_exactly_one_button() {
        let base = vec!["home".to_owned(); 20];
        let welcome = render_over(20, 60, &base, ExitChoice::Welcome).join("\n");
        let quit = render_over(20, 60, &base, ExitChoice::Quit).join("\n");
        let stay = render_over(20, 60, &base, ExitChoice::Stay).join("\n");
        // Each focus paints its own button bold in its own role colour, and the
        // other two stay dimmed.
        assert!(welcome.contains("\u{1b}[1;36m[ welcome ]"));
        assert!(quit.contains("\u{1b}[1;31m[ quit    ]"));
        assert!(stay.contains("\u{1b}[1;33m[ stay    ]"));
        assert!(!welcome.contains("\u{1b}[1;31m[ quit    ]"));
        assert!(!quit.contains("\u{1b}[1;36m[ welcome ]"));
        assert_ne!(welcome, quit);
        assert_ne!(quit, stay);
    }
}

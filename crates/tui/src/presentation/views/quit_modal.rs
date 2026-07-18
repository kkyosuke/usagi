//! Detach / quit confirmation overlay.
//!
//! A stateless renderer for the controller's `Overlay::QuitConfirmation`. The
//! reducer owns the decision (`y`/Enter detaches, `n`/Esc stays), so this
//! surface only draws the prompt the shell composites over a `render_home`
//! frame.

use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::modal;

const INNER_WIDTH: usize = 40;
const BODY_HEIGHT: usize = 3;

/// Render the quit confirmation over an existing Home frame without replacing
/// its background.
#[must_use]
pub fn render_over(height: usize, width: usize, base: &[String]) -> Vec<String> {
    let body = modal::fixed_body(
        vec![
            Style::new().paint("  Detach from this workspace?"),
            String::new(),
            format!(
                "  {}    {}",
                Role::Danger.style().bold().paint("y: detach"),
                Style::new().dim().paint("n / Esc: stay"),
            ),
        ],
        BODY_HEIGHT,
    );
    modal::render_over(height, width, base, "Quit", INNER_WIDTH, &body)
}

#[cfg(test)]
mod tests {
    use super::render_over;
    use crate::presentation::widgets::display_width;

    #[test]
    fn draws_the_detach_prompt_over_the_frame() {
        let base = vec!["home".to_owned(); 20];
        let frame = render_over(20, 60, &base);
        let text = frame.join("\n");
        assert!(text.contains("Quit"));
        assert!(text.contains("Detach from this workspace?"));
        assert!(text.contains("y: detach"));
        assert!(text.contains("n / Esc: stay"));
        assert!(frame.iter().all(|line| display_width(line) <= 60));
    }
}

//! Session-create failure dialog overlay.
//!
//! A stateless renderer for the controller's [`Overlay::CreateSessionError`].
//! The reducer opens it when a create request the daemon accepted later fails,
//! and owns dismissal (`Enter` / `Esc` / `Ctrl-C`). This surface only draws the
//! safe message the reducer attached, composited over a `render_home` frame; it
//! is the create-failure counterpart to [`quit_modal`](super::quit_modal).
//!
//! Only the safe [`Notice`] message reaches this view — raw protocol, internal,
//! or secret detail is collapsed to a safe single line upstream, so there is no
//! unsafe payload to leak here.

use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::{self, modal};

const INNER_WIDTH: usize = 54;
const BODY_HEIGHT: usize = 4;

/// Render the create-failure dialog over an existing Home frame without
/// replacing its background. `message` is the reducer's safe notice text.
#[must_use]
pub fn render_over(height: usize, width: usize, base: &[String], message: &str) -> Vec<String> {
    let body = modal::fixed_body(
        vec![
            Role::Danger.style().bold().paint(&widgets::clip_to_width(
                &format!("  {message}"),
                INNER_WIDTH,
            )),
            String::new(),
            Style::new().dim().paint("  Enter / Esc: dismiss"),
        ],
        BODY_HEIGHT,
    );
    modal::render_over(
        height,
        width,
        base,
        "Session create failed",
        INNER_WIDTH,
        &body,
    )
}

#[cfg(test)]
mod tests {
    use super::render_over;
    use crate::presentation::widgets::display_width;
    use crate::usecase::application::controller::{
        AppEvent, AppKey, AppState, Effect, Notice, OperationResult, Overlay, update,
    };
    use usagi_core::domain::id::WorkspaceId;

    fn joined(frame: &[String]) -> String {
        frame.join("\n")
    }

    #[test]
    fn draws_the_safe_failure_message_over_the_frame() {
        let base = vec!["home".to_owned(); 24];
        let frame = render_over(24, 80, &base, "worktree path already exists");
        let text = joined(&frame);
        assert!(text.contains("Session create failed"));
        assert!(text.contains("worktree path already exists"));
        assert!(text.contains("Enter / Esc: dismiss"));
        assert!(frame.iter().all(|line| display_width(line) <= 80));
    }

    #[test]
    fn fits_a_narrow_terminal_and_clips_a_long_message() {
        let base = vec![String::new(); 16];
        let long = "x".repeat(200);
        let frame = render_over(16, 20, &base, &long);
        assert!(frame.iter().all(|line| display_width(line) <= 20));
    }

    /// The reducer opens this overlay on a failed create, and the shell keys the
    /// render off the same message, so a failure end-to-end shows the dialog.
    #[test]
    fn reducer_failure_populates_the_message_this_view_draws() {
        let mut state = AppState::home(WorkspaceId::new(), Vec::new());
        let _ = update(&mut state, AppEvent::Key(AppKey::CtrlA));
        for character in ['a', 'p', 'i'] {
            let _ = update(&mut state, AppEvent::Key(AppKey::Char(character)));
        }
        let effects = update(&mut state, AppEvent::Key(AppKey::Enter));
        let token = match &effects[..] {
            [Effect::CreateSession { token, .. }] => *token,
            _ => panic!("expected a create effect"),
        };
        let _ = update(
            &mut state,
            AppEvent::OperationResult(OperationResult {
                token,
                succeeded: false,
                created: None,
                notice: Some(Notice::new("daemon rejected the request")),
            }),
        );
        assert_eq!(state.overlay(), Some(Overlay::CreateSessionError));
        let message = state.create_session_error().unwrap().message.clone();
        let base = vec![String::new(); 24];
        let frame = render_over(24, 80, &base, &message);
        assert!(joined(&frame).contains("daemon rejected the request"));
    }
}

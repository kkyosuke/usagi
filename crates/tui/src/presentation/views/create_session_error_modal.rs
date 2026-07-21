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
//! unsafe payload to leak here. That safe line is **wrapped** to the dialog width
//! and shown in full rather than clipped, so a longer safe message stays readable.

use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::{self, modal};

const INNER_WIDTH: usize = 54;

/// Render the create-failure dialog over an existing Home frame without
/// replacing its background. `message` is the reducer's safe notice text; it is
/// wrapped to the dialog's inner width (leaving a two-column indent that matches
/// the dismiss hint) so every character of the safe message is shown across as
/// many rows as it needs, and the box grows to fit instead of clipping.
#[must_use]
pub fn render_over(height: usize, width: usize, base: &[String], message: &str) -> Vec<String> {
    // Wrap against the width the modal will actually use, so even a narrow
    // terminal shows the whole safe message rather than a clipped first line.
    let (_, normalized_width) = widgets::normalize_size(height, width);
    let inner_width = modal::modal_inner_width(normalized_width, INNER_WIDTH);
    // Each message row carries a two-column left indent (matching the dismiss
    // hint). `boxed` then adds one box column on each side, so a row that fills
    // `inner_width` would sit three columns from the left border but only one
    // from the right. Reserve the same indent width on the right by wrapping to
    // `inner_width - 2 * BODY_INDENT_WIDTH`, so the widest row keeps symmetric
    // three-column padding on both sides.
    let wrap_width = inner_width.saturating_sub(2 * modal::BODY_INDENT_WIDTH);

    let danger = Role::Danger.style().bold();
    // Wrap the plain message, then paint each row: `wrap_to_width` measures ANSI
    // escapes as visible columns, so styling must come after wrapping.
    let mut body: Vec<String> = widgets::wrap_to_width(message, wrap_width)
        .into_iter()
        .map(|segment| danger.paint(&format!("  {segment}")))
        .collect();
    if body.is_empty() {
        body.push(String::new());
    }
    body.push(String::new());
    body.push(Style::new().dim().paint("  Enter / Esc: dismiss"));

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
    #![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract
    use super::{INNER_WIDTH, render_over};
    use crate::presentation::widgets::{
        display_width, modal, normalize_size, strip_ansi, wrap_to_width,
    };
    use crate::usecase::application::controller::{
        AppEvent, AppKey, AppState, Effect, Notice, OperationResult, Overlay, update,
    };
    use usagi_core::domain::id::WorkspaceId;

    fn joined(frame: &[String]) -> String {
        frame.join("\n")
    }

    /// The wrap width the view uses for a given terminal size, mirrored so tests
    /// can assert against the exact segments the dialog draws.
    fn wrap_width_for(height: usize, width: usize) -> usize {
        let (_, normalized) = normalize_size(height, width);
        modal::modal_inner_width(normalized, INNER_WIDTH)
            .saturating_sub(2 * modal::BODY_INDENT_WIDTH)
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
    fn wraps_a_long_message_across_rows_and_shows_all_of_it() {
        // A safe message longer than one dialog row must wrap and stay fully
        // visible — no single-line clip, no dropped tail.
        let message = "worktree path already exists and could not be reused for the new session";
        let base = vec!["home".to_owned(); 24];
        let frame = render_over(24, 80, &base, message);
        let text = joined(&frame);

        let segments = wrap_to_width(message, wrap_width_for(24, 80));
        assert!(segments.len() >= 2, "expected the message to wrap");
        // Every wrapped segment appears in the rendered dialog, so the whole
        // message is shown rather than truncated.
        for segment in &segments {
            assert!(
                text.contains(segment.trim_end()),
                "missing wrapped segment: {segment:?}"
            );
        }
        // The box grew to hold each message row: no ellipsis clip leaked in.
        assert!(!text.contains('…'));
        assert!(frame.iter().all(|line| display_width(line) <= 80));
    }

    /// The interior padding of a rendered box row: spaces between the left `│`
    /// and the first non-space glyph, and between the last non-space glyph and
    /// the right `│`. ANSI is stripped first so only visible columns count.
    fn interior_padding(row: &str) -> (usize, usize) {
        let plain = strip_ansi(row);
        let left = plain.find('│').expect("row has a left border");
        let right = plain.rfind('│').expect("row has a right border");
        let interior: &str = &plain[left + '│'.len_utf8()..right];
        let leading = interior.len() - interior.trim_start_matches(' ').len();
        let trailing = interior.len() - interior.trim_end_matches(' ').len();
        (leading, trailing)
    }

    #[test]
    fn keeps_symmetric_left_and_right_padding_on_the_widest_wrapped_row() {
        // A message long enough to wrap and fill the box width. The old wrap of
        // `inner_width - 2` let a filled row touch the right border (3 columns of
        // left padding, 1 of right); the symmetric wrap keeps both sides equal.
        let message = "x".repeat(200);
        let base = vec![String::new(); 24];
        let frame = render_over(24, 80, &base, &message);

        // The rows carrying the wrapped message are the ones with box content.
        let message_rows: Vec<&String> = frame
            .iter()
            .filter(|row| strip_ansi(row).contains('x'))
            .collect();
        assert!(message_rows.len() >= 2, "expected the message to wrap");

        let widest = message_rows
            .iter()
            .map(|row| interior_padding(row))
            .max_by_key(|&(left, _)| left)
            .unwrap();
        // The message rows are drawn at the widest fill, so left and right
        // interior padding match: box column (1) + body indent (2) on each side.
        for row in &message_rows {
            let (left, right) = interior_padding(row);
            // Left and right interior padding match: box column (1) + body
            // indent (2) on each side.
            assert_eq!(left, 1 + modal::BODY_INDENT_WIDTH);
            assert_eq!(right, left);
        }
        assert_eq!(widest.0, widest.1);
    }

    #[test]
    fn draws_the_dialog_without_a_message_row_when_the_message_is_empty() {
        // An empty safe message leaves no wrapped rows; the dialog still opens
        // with its title and dismiss hint rather than collapsing.
        let base = vec!["home".to_owned(); 24];
        let frame = render_over(24, 80, &base, "");
        let text = joined(&frame);
        assert!(text.contains("Session create failed"));
        assert!(text.contains("Enter / Esc: dismiss"));
        assert!(frame.iter().all(|line| display_width(line) <= 80));
    }

    #[test]
    fn fits_a_narrow_terminal_without_overflow() {
        let base = vec![String::new(); 16];
        let long = "x".repeat(200);
        let frame = render_over(16, 20, &base, &long);
        assert!(frame.iter().all(|line| display_width(line) <= 20));
        // Even a very narrow dialog wraps rather than showing a single row.
        assert!(wrap_width_for(16, 20) >= 1);
    }

    /// The reducer opens this overlay on a failed create, and the shell keys the
    /// render off the same message, so a failure end-to-end shows the dialog.
    // The submit match keeps an unreachable panic arm for a clear failure; the
    // covered render paths live in the two tests above.
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

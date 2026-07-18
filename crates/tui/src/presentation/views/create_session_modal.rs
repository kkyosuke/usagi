//! New-session creation form overlay.
//!
//! A pure renderer over the controller-owned [`CreateSessionForm`] draft. Field
//! edits, submission, and validation are controller effects; this surface only
//! draws the three fields, a caret on the active one, and any error the reducer
//! attached. It is the `Overlay::CreateSession` counterpart the shell composites
//! over a `render_home` frame, replacing the legacy inline sidebar create row.

use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::{self, modal};
use crate::usecase::application::controller::{CreateSessionField, CreateSessionForm};

const INNER_WIDTH: usize = 54;
const BODY_HEIGHT: usize = 9;

fn field_line(label: &str, value: &str, active: bool) -> String {
    let accent = Role::Accent.style().bold();
    let marker = if active {
        accent.paint("›")
    } else {
        " ".to_owned()
    };
    let rendered = if active {
        widgets::block_caret(value, value.chars().count(), &accent)
    } else if value.is_empty() {
        Style::new().dim().paint("(workspace default)")
    } else {
        value.to_owned()
    };
    widgets::clip_to_width(&format!("{marker} {label}: {rendered}"), INNER_WIDTH)
}

fn body(form: &CreateSessionForm) -> Vec<String> {
    let mut lines = vec![
        Style::new()
            .dim()
            .paint("  Tab: next field   Enter: create   Esc: cancel"),
        String::new(),
        field_line(
            "name   ",
            form.name(),
            form.field() == CreateSessionField::Name,
        ),
        field_line(
            "profile",
            form.profile(),
            form.field() == CreateSessionField::Profile,
        ),
        field_line(
            "model  ",
            form.model(),
            form.field() == CreateSessionField::Model,
        ),
    ];
    if let Some(error) = form.error() {
        lines.push(String::new());
        lines.push(
            Role::Danger
                .style()
                .bold()
                .paint(&format!("  {}", error.message)),
        );
    }
    modal::fixed_body(lines, BODY_HEIGHT)
}

/// Render the new-session form over an existing Home frame without replacing its
/// background.
#[must_use]
pub fn render_over(
    height: usize,
    width: usize,
    base: &[String],
    form: &CreateSessionForm,
) -> Vec<String> {
    modal::render_over(height, width, base, "New session", INNER_WIDTH, &body(form))
}

#[cfg(test)]
mod tests {
    use super::render_over;
    use crate::presentation::widgets::display_width;
    use crate::usecase::application::controller::{AppEvent, AppKey, AppState, Overlay, update};
    use usagi_core::domain::id::WorkspaceId;

    fn open_create_form() -> AppState {
        let mut state = AppState::home(WorkspaceId::new(), Vec::new());
        let _ = update(&mut state, AppEvent::Key(AppKey::Down)); // + new session
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter)); // open create form
        assert_eq!(state.overlay(), Some(Overlay::CreateSession));
        state
    }

    fn joined(frame: &[String]) -> String {
        frame.join("\n")
    }

    #[test]
    fn renders_typed_values_the_active_caret_and_default_placeholders() {
        let mut state = open_create_form();
        // Type a name, then tab twice to the (empty) model field. This renders the
        // three field states in one frame: name is inactive with a value, profile
        // is inactive and empty (default placeholder), model is the active caret.
        for character in ['a', 'p', 'i'] {
            let _ = update(&mut state, AppEvent::Key(AppKey::Char(character)));
        }
        let _ = update(&mut state, AppEvent::Key(AppKey::Tab));
        let _ = update(&mut state, AppEvent::Key(AppKey::Tab));

        let base = vec![String::new(); 24];
        let frame = render_over(24, 80, &base, state.create_session_form().unwrap());
        let text = joined(&frame);
        assert!(text.contains("New session"));
        assert!(text.contains("api")); // inactive, non-empty name value
        assert!(text.contains("(workspace default)")); // inactive, empty profile/model
        assert!(frame.iter().all(|line| display_width(line) <= 80));
    }

    #[test]
    fn renders_a_validation_error_when_the_reducer_attaches_one() {
        let mut state = open_create_form();
        // Submit with an empty name: the reducer keeps the form and attaches an error.
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        assert_eq!(state.overlay(), Some(Overlay::CreateSession));
        let form = state.create_session_form().unwrap();
        assert!(form.error().is_some());
        let base = vec![String::new(); 24];
        let frame = render_over(24, 80, &base, form);
        assert!(joined(&frame).contains("New session"));
    }
}

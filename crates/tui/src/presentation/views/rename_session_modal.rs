//! Display-name editor over the Switch surface.

use crate::presentation::theme::Role;
use crate::presentation::widgets::{self, modal};
use crate::usecase::application::controller::RenameSessionForm;

#[must_use]
pub fn render_over(
    height: usize,
    width: usize,
    base: &[String],
    form: &RenameSessionForm,
) -> Vec<String> {
    let inner_width = modal::modal_inner_width(width, 54);
    let field_width = inner_width.saturating_sub(4);
    let mut body = vec!["  Display name".to_owned()];
    body.extend(
        widgets::wrap_with_trailing_caret(&form.name, field_width, &Role::Accent.style())
            .into_iter()
            .map(|line| format!("  {line}")),
    );
    body.push(String::new());
    if let Some(error) = &form.error {
        body.extend(
            widgets::wrap_to_width(&error.message, field_width)
                .into_iter()
                .map(|line| Role::Danger.style().paint(&format!("  {line}"))),
        );
    }
    body.push(if form.saving {
        "  Saving…".to_owned()
    } else {
        "  Enter: save   Esc: cancel".to_owned()
    });
    modal::render_over(height, width, base, "Rename session", 54, &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usecase::application::controller::{
        AppEvent, AppKey, AppState, BackendEvent, Notice, update,
    };
    use std::collections::BTreeMap;
    use usagi_core::domain::id::{SessionId, WorkspaceId};

    #[test]
    fn rename_dialog_renders_draft_saving_and_error_within_terminal_width() {
        let session = SessionId::new();
        let mut state = AppState::home(WorkspaceId::new(), vec![session]);
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::SessionLabels(BTreeMap::from([(
                session,
                "名前👩‍💻".into(),
            )]))),
        );
        let _ = update(&mut state, AppEvent::Key(AppKey::Char('r')));
        let mut form = state.rename_session_form().unwrap().clone();
        let base = vec!["background".to_owned(); 24];
        let frame = render_over(24, 80, &base, &form);
        assert!(frame.join("\n").contains("Rename session"));
        assert!(frame.join("\n").contains("名前👩‍💻"));
        assert!(frame.join("\n").contains("Enter: save"));
        form.saving = true;
        assert!(
            render_over(24, 80, &base, &form)
                .join("\n")
                .contains("Saving…")
        );
        form.saving = false;
        form.error = Some(Notice::new("Could not save. Please retry."));
        form.name = "名前".repeat(32);
        for width in [1, 20, 40, 80] {
            let frame = render_over(24, width, &base, &form);
            assert!(
                frame
                    .iter()
                    .all(|line| widgets::display_width(line) <= width)
            );
        }
        assert!(
            render_over(24, 80, &base, &form)
                .join("\n")
                .contains("Please retry.")
        );
    }
}

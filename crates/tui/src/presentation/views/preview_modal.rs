//! File finder and read-only text viewer for the Preview overlay.

use crate::presentation::theme::{Role, Style};
use crate::presentation::views::text_overlay::{self, OverlayDocument, TextOverlay};
use crate::presentation::widgets::modal;
use crate::usecase::application::controller::PreviewOverlay;

const INNER_WIDTH: usize = 76;
const BODY_HEIGHT: usize = 14;
const FILE_ROWS: usize = 10;

/// Compose Preview over an existing Home frame.
#[must_use]
pub fn render_over(
    height: usize,
    width: usize,
    base: &[String],
    state: &PreviewOverlay,
) -> Vec<String> {
    if let Some(path) = state.path() {
        return render_document(height, width, base, state, path);
    }
    render_finder(height, width, base, state)
}

fn render_finder(
    height: usize,
    width: usize,
    base: &[String],
    state: &PreviewOverlay,
) -> Vec<String> {
    let inner = modal::modal_inner_width(width, INNER_WIDTH);
    let visible = state.visible_files();
    let rows = visible
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let marker = modal::selection_marker(index == state.selected());
            let row = format!("{marker} {path}");
            let row = if index == state.selected() {
                Role::Accent.style().bold().paint(&row)
            } else {
                row
            };
            modal::content_line(&row, inner)
        })
        .collect::<Vec<_>>();

    let mut body = vec![modal::filter_line(
        state.filter(),
        state.filter().len(),
        None,
    )];
    if state.is_loading() {
        body.push(modal::empty_notice("Loading files…"));
    } else if let Some(error) = state.error() {
        body.push(modal::error_line(error.message.as_str(), inner));
    } else if rows.is_empty() {
        body.push(modal::empty_notice(if state.filter().is_empty() {
            "No files available."
        } else {
            "No files match the filter."
        }));
    } else {
        body.extend(modal::bounded_list_rows(&rows, state.selected(), FILE_ROWS));
    }
    body.push(modal::footer(
        "type fuzzy filter / ↑↓ select / Enter preview / Esc close",
    ));
    modal::render_body_over(
        height,
        width,
        base,
        "Preview files",
        inner,
        BODY_HEIGHT,
        body,
    )
}

fn render_document(
    height: usize,
    width: usize,
    base: &[String],
    state: &PreviewOverlay,
    path: &str,
) -> Vec<String> {
    let document = state.error().map_or_else(
        || {
            if state.is_loading() {
                OverlayDocument::Ready(vec![Style::new().dim().paint("Loading preview…")])
            } else {
                OverlayDocument::Ready(state.lines().to_vec())
            }
        },
        |error| OverlayDocument::Unavailable(error.message.as_str().to_owned()),
    );
    text_overlay::render_over(
        height,
        width,
        base,
        &TextOverlay::new(format!("Preview · {path}"), document)
            .scrolled_to(state.scroll())
            .with_footer("↑↓ scroll   Esc: back to files"),
    )
}

#[cfg(test)]
mod tests {
    use usagi_core::domain::id::{SessionId, WorkspaceId};

    use super::*;
    use crate::presentation::widgets::{display_width, strip_ansi};
    use crate::usecase::application::controller::{
        AppEvent, AppKey, AppState, BackendEvent, SafeError, SafeMessage, Target, update,
    };

    fn joined(state: &AppState) -> String {
        let base = vec!["background".to_owned(); 24];
        render_over(24, 90, &base, state.preview_overlay().unwrap())
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn finder_and_document_render_their_controls_and_states() {
        let workspace = WorkspaceId::new();
        let target = Target::Session(SessionId::new());
        let mut state = AppState::home(workspace, vec![target.session_id().unwrap()]);
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenPreview));
        assert!(joined(&state).contains("Loading files"));

        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::PreviewLoaded {
                target,
                path: None,
                files: vec!["README.md".into(), "src/main.rs".into()],
                lines: vec![],
            }),
        );
        let finder = joined(&state);
        assert!(finder.contains("Preview files"));
        assert!(finder.contains("README.md"));
        assert!(finder.contains("type fuzzy filter"));

        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        assert!(joined(&state).contains("Loading preview"));
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::PreviewLoaded {
                target,
                path: Some("README.md".into()),
                files: vec![],
                lines: vec!["hello".into()],
            }),
        );
        let document = joined(&state);
        assert!(document.contains("Preview · README.md"));
        assert!(document.contains("hello"));
        assert!(document.contains("back to files"));
    }

    #[test]
    fn finder_empty_filter_and_error_states_stay_inside_the_frame() {
        let workspace = WorkspaceId::new();
        let target = Target::Session(SessionId::new());
        let mut state = AppState::home(workspace, vec![target.session_id().unwrap()]);
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenPreview));
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::PreviewLoaded {
                target,
                path: None,
                files: vec![],
                lines: vec![],
            }),
        );
        assert!(joined(&state).contains("No files available"));
        let _ = update(&mut state, AppEvent::Key(AppKey::Char('x')));
        assert!(joined(&state).contains("No files match"));

        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::PreviewError {
                target,
                path: None,
                error: SafeError {
                    message: SafeMessage::new("files unavailable"),
                    error_id: "preview-files".into(),
                },
            }),
        );
        let frame = render_over(
            9,
            30,
            &vec!["background".into(); 9],
            state.preview_overlay().unwrap(),
        );
        assert!(frame.iter().all(|line| display_width(line) <= 30));
    }
}

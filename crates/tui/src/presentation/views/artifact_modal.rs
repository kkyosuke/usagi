//! Workspace artifacts presented in the same compact list shape as Pull Requests.

use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::{self, modal};
use crate::usecase::application::controller::PreviewOverlay;

const INNER_WIDTH: usize = 88;
const BODY_HEIGHT: usize = 15;

/// Project the validated storage path into session, retention state, and name.
fn columns(path: &str) -> (&str, &str, &str) {
    if let Some(relative) = path.strip_prefix(".usagi/sessions/")
        && let Some((session, name)) = relative.split_once("/artifacts/")
    {
        return (session, "active", name);
    }
    if let Some(relative) = path.strip_prefix(".usagi/artifacts/")
        && let Some((session, snapshot)) = relative.split_once('/')
        && let Some((_, name)) = snapshot.split_once('/')
    {
        return (session, "retained", name);
    }
    (
        "this session",
        "active",
        path.strip_prefix("artifacts/").unwrap_or(path),
    )
}

fn row(path: &str, selected: bool, inner: usize) -> String {
    let (session, status, name) = columns(path);
    let session_width = 16.min(inner / 3);
    let session = widgets::pad_to_width(
        &widgets::clip_to_width(session, session_width),
        session_width,
    );
    let status = if status == "retained" {
        Role::Feature.style()
    } else {
        Role::Success.style()
    }
    .paint(&format!("{status:<9}"));
    let name = Role::Accent.style().bold().paint(name);
    modal::content_line(
        &format!(
            "{} {session} {status} {name}",
            modal::selection_marker(selected)
        ),
        inner,
    )
}

fn body(state: &PreviewOverlay, height: usize, inner: usize) -> Vec<String> {
    let mut lines = Vec::new();
    if height >= 4 {
        lines.push(modal::caption(state.file_filter().label()));
        lines.push(format!(
            "{}  {}",
            modal::filter_line(state.filter(), state.filter().len(), None),
            Style::new().dim().paint(&format!(
                "{}/{} artifacts",
                state.visible_len(),
                state.total_files()
            )),
        ));
        if height >= 6 {
            lines.push(modal::caption("session          state      artifact"));
        }
    }
    let detail = height >= 7 && state.selected_file().is_some();
    let capacity = height.saturating_sub(lines.len() + 1 + usize::from(detail));
    if capacity > 0 {
        if state.is_loading() {
            lines.push(modal::empty_notice("Loading artifacts…"));
        } else if let Some(error) = state.error() {
            lines.push(modal::error_line(error.message.as_str(), inner));
        } else if state.visible_len() == 0 {
            lines.push(modal::empty_notice(if state.filter().is_empty() {
                "No artifacts yet. Save generated files in artifacts/."
            } else {
                "No matching artifacts. Backspace edits the filter."
            }));
        } else {
            let rows = state
                .visible_candidates()
                .iter()
                .enumerate()
                .map(|(index, candidate)| row(candidate.path(), index == state.selected(), inner))
                .collect::<Vec<_>>();
            lines.extend(modal::bounded_list_rows(&rows, state.selected(), capacity));
        }
    }
    if detail {
        // The full storage location distinguishes repeated snapshots of a file.
        lines.push(modal::content_line(
            &Style::new().dim().paint(state.selected_file().unwrap()),
            inner,
        ));
    }
    if height > 0 {
        lines.push(modal::footer(
            "↑↓ select / Enter open / Tab preview / type filter / Esc close",
        ));
    }
    lines
}

/// Compose a session or workspace artifact list over the Home frame.
#[must_use]
pub fn render_over(
    height: usize,
    width: usize,
    base: &[String],
    state: &PreviewOverlay,
) -> Vec<String> {
    let inner = modal::modal_inner_width(width, INNER_WIDTH);
    let body_height = modal::reserved_body_height(height, width, BODY_HEIGHT);
    modal::render_body_over(
        height,
        width,
        base,
        "Artifacts",
        INNER_WIDTH,
        BODY_HEIGHT,
        body(state, body_height, inner),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::widgets::{display_width, strip_ansi};
    use crate::usecase::application::controller::{
        AppEvent, AppKey, AppState, BackendEvent, PreviewFileFilter, SafeError, SafeMessage,
        Target, update,
    };
    use usagi_core::domain::id::{SessionId, WorkspaceId};

    fn loaded(paths: Vec<String>) -> AppState {
        let workspace = WorkspaceId::new();
        let mut state = AppState::home(workspace, vec![SessionId::new()]);
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
        let _ = update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("artifact".into())),
        );
        let request_id = state.preview_overlay().unwrap().request_id();
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::PreviewLoaded {
                target: Target::Root(workspace),
                request_id,
                path: None,
                filter: PreviewFileFilter::AllArtifacts,
                files: paths,
                changed: Vec::new(),
                lines: Vec::new(),
            }),
        );
        state
    }

    fn frame(state: &AppState, height: usize, width: usize) -> Vec<String> {
        render_over(
            height,
            width,
            &vec!["background".repeat(width); height],
            state.preview_overlay().unwrap(),
        )
    }

    fn text(state: &AppState) -> String {
        frame(state, 24, 100)
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn storage_paths_show_session_retention_and_artifact_names() {
        assert_eq!(
            columns("artifacts/design.md"),
            ("this session", "active", "design.md")
        );
        assert_eq!(columns("odd.md"), ("this session", "active", "odd.md"));
        let state = loaded(vec![
            ".usagi/sessions/alpha/artifacts/design.md".into(),
            ".usagi/artifacts/beta/00000000-0000-4000-8000-000000000000/chart.png".into(),
        ]);
        let rendered = text(&state);
        for expected in [
            "Artifacts",
            "alpha",
            "beta",
            "active",
            "retained",
            "design.md",
            "chart.png",
            "Enter open",
            "Tab preview",
            "2/2 artifacts",
        ] {
            assert!(rendered.contains(expected), "{rendered}");
        }
        assert!(state.preview_overlay().unwrap().pane().path().is_none());
    }

    #[test]
    fn selection_scrolls_within_the_box_and_keeps_controls_visible() {
        let mut state = loaded(
            (0..30)
                .map(|index| format!(".usagi/sessions/調査/artifacts/report-{index:02}.md"))
                .collect(),
        );
        for _ in 0..29 {
            let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        }
        let rendered = text(&state);
        assert!(rendered.contains("report-29.md"));
        assert!(rendered.contains("↑"));
        assert!(rendered.contains("Enter open"));
        let narrow = frame(&state, 12, 50);
        assert_eq!(narrow.len(), 12);
        assert!(narrow.iter().all(|line| display_width(line) == 50));
        assert!(narrow.join("\n").contains("Enter open"));
        for height in [0, 4, 8, 9, 24] {
            let _ = frame(&state, height, 100);
        }
        assert!(body(state.preview_overlay().unwrap(), 0, 40).is_empty());
        assert!(
            body(state.preview_overlay().unwrap(), 1, 40)
                .join("\n")
                .contains("Enter open")
        );
    }

    #[test]
    fn empty_loading_failed_and_filtered_lists_have_distinct_messages() {
        let mut state = loaded(Vec::new());
        assert!(text(&state).contains("Save generated files in artifacts/"));
        let _ = update(&mut state, AppEvent::Key(AppKey::Char('x')));
        assert!(text(&state).contains("No matching artifacts"));
        let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
        let _ = update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("artifact".into())),
        );
        assert!(text(&state).contains("Loading artifacts"));
        let overlay = state.preview_overlay().unwrap();
        let target = overlay.target();
        let request_id = overlay.request_id();
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::PreviewError {
                target,
                request_id,
                path: None,
                filter: PreviewFileFilter::AllArtifacts,
                error: SafeError {
                    message: SafeMessage::new("Artifacts unavailable"),
                    error_id: "artifact-list".into(),
                },
            }),
        );
        assert!(text(&state).contains("Artifacts unavailable"));
    }
}

//! Note scratchpad and environment editor overlays.
//!
//! Both surfaces are pure renderers over controller-owned draft state. Their
//! persistence is deliberately handled by controller effects, keeping the
//! workspace state and settings owners outside presentation.

use crate::presentation::theme::Role;
use crate::presentation::widgets::modal;
use crate::usecase::application::controller::{EnvironmentEditor, NoteEditor, NoteSection};

const INNER_WIDTH: usize = 62;
const MAX_ROWS: usize = 8;
const NOTES_BODY_HEIGHT: usize = 16;
const ENVIRONMENT_BODY_HEIGHT: usize = 14;

fn error_line(error: Option<&str>) -> Option<String> {
    error.map(|message| {
        Role::Danger
            .style()
            .bold()
            .paint(&format!("  Error: {message}"))
    })
}

fn note_body(editor: &NoteEditor) -> Vec<String> {
    let mut lines = vec![modal::caption("note · todos · decisions")];
    let section = match editor.section() {
        NoteSection::Note => "note",
        NoteSection::Todos => "todos",
        NoteSection::Decisions => "decisions",
    };
    lines.push(modal::heading(&format!("[{section}]")));
    match editor.section() {
        NoteSection::Note => lines.extend(
            editor
                .scratchpad()
                .note()
                .unwrap_or("(empty)")
                .lines()
                .take(MAX_ROWS)
                .map(|line| modal::content_line(line, INNER_WIDTH)),
        ),
        NoteSection::Todos => {
            if editor.scratchpad().todos().is_empty() {
                lines.push(modal::empty_notice("(no todos)"));
            }
            lines.extend(
                editor
                    .scratchpad()
                    .todos()
                    .iter()
                    .take(MAX_ROWS)
                    .map(|todo| {
                        let mark = if todo.done { "x" } else { " " };
                        modal::content_line(&format!("[{mark}] {}", todo.text), INNER_WIDTH)
                    }),
            );
        }
        NoteSection::Decisions => {
            if editor.scratchpad().decisions().is_empty() {
                lines.push(modal::empty_notice("(no decisions)"));
            }
            lines.extend(
                editor
                    .scratchpad()
                    .decisions()
                    .iter()
                    .rev()
                    .take(MAX_ROWS)
                    .map(|decision| {
                        modal::content_line(
                            &format!(
                                "{}  {}",
                                decision.at.format("%Y-%m-%d %H:%M"),
                                decision.text
                            ),
                            INNER_WIDTH,
                        )
                    }),
            );
        }
    }
    if !editor.draft().is_empty() {
        lines.push(String::new());
        lines.push(
            Role::Warning
                .style()
                .paint(&format!("  draft: {}", editor.draft())),
        );
    }
    if let Some(line) = error_line(editor.error().map(|error| error.message.as_str())) {
        lines.push(String::new());
        lines.push(line);
    }
    lines.push(String::new());
    lines.push(modal::footer("Esc: close   Save: persist"));
    modal::fixed_body(lines, NOTES_BODY_HEIGHT)
}

fn environment_body(editor: &EnvironmentEditor) -> Vec<String> {
    let mut lines = vec![modal::caption("workspace / session environment")];
    if editor.entries().is_empty() {
        lines.push(modal::empty_notice("(no environment variables)"));
    }
    lines.extend(
        editor.entries().iter().take(MAX_ROWS).map(|entry| {
            modal::content_line(&format!("{}={}", entry.name, entry.value), INNER_WIDTH)
        }),
    );
    if editor.entries().len() > MAX_ROWS {
        lines.push(modal::caption(&format!(
            "… {} more",
            editor.entries().len() - MAX_ROWS
        )));
    }
    if let Some(line) = error_line(editor.error().map(|error| error.message.as_str())) {
        lines.push(String::new());
        lines.push(line);
    }
    lines.push(String::new());
    lines.push(modal::footer("Esc: close   Save: persist"));
    modal::fixed_body(lines, ENVIRONMENT_BODY_HEIGHT)
}

/// Render the scratchpad over an existing Home frame without replacing its background.
#[must_use]
pub fn render_notes_over(
    height: usize,
    width: usize,
    base: &[String],
    editor: &NoteEditor,
) -> Vec<String> {
    modal::render_over(
        height,
        width,
        base,
        "Notes",
        INNER_WIDTH,
        &note_body(editor),
    )
}

/// Render the environment editor over an existing Home frame without replacing its background.
#[must_use]
pub fn render_environment_over(
    height: usize,
    width: usize,
    base: &[String],
    editor: &EnvironmentEditor,
) -> Vec<String> {
    modal::render_over(
        height,
        width,
        base,
        "Environment",
        INNER_WIDTH,
        &environment_body(editor),
    )
}

#[cfg(test)]
mod tests {
    use super::{render_environment_over, render_notes_over};
    use crate::presentation::widgets::display_width;
    use crate::usecase::application::controller::{
        AppEvent, AppKey, AppState, BackendEvent, EnvironmentEntry, NoteSection, SafeError,
        SafeMessage, Target, update,
    };
    use chrono::{TimeZone, Utc};
    use usagi_core::domain::id::WorkspaceId;
    use usagi_core::domain::note::{Scratchpad, SessionDecision, SessionTodo};

    fn base() -> Vec<String> {
        (0..24)
            .map(|row| format!("home-row-{row}-{}", ".".repeat(72)))
            .collect()
    }

    #[test]
    fn overlays_keep_background_visible_and_render_editor_values() {
        let workspace = WorkspaceId::new();
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenNotes));
        let empty_notes = render_notes_over(0, 0, &base(), state.note_editor().unwrap());
        let notes_height = empty_notes
            .iter()
            .filter(|line| line.contains('│') || line.contains('┌') || line.contains('└'))
            .count();
        assert!(empty_notes.join("\n").contains("(empty)"));
        assert_eq!(empty_notes.len(), 24);
        for (section, expected) in [
            (NoteSection::Todos, "no todos"),
            (NoteSection::Decisions, "no decisions"),
        ] {
            let _ = update(
                &mut state,
                AppEvent::Key(AppKey::SelectNoteSection(section)),
            );
            assert!(
                render_notes_over(24, 80, &base(), state.note_editor().unwrap())
                    .join("\n")
                    .contains(expected)
            );
        }
        let _ = update(
            &mut state,
            AppEvent::Key(AppKey::SelectNoteSection(NoteSection::Note)),
        );
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::NotesLoaded {
                target: Target::Root(workspace),
                scratchpad: Scratchpad {
                    note: Some("remember this\nand this".into()),
                    todos: vec![SessionTodo::new("first"), SessionTodo::new("second")],
                    decisions: vec![SessionDecision::new(
                        Utc.with_ymd_and_hms(2026, 7, 13, 12, 0, 0).unwrap(),
                        "keep the port boundary",
                    )],
                },
            }),
        );
        let _ = update(
            &mut state,
            AppEvent::Key(AppKey::SetNoteDraft("draft survives".into())),
        );
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::NotesError {
                target: Target::Root(workspace),
                error: SafeError {
                    message: SafeMessage::new("Could not save notes"),
                    error_id: "safe-notes".into(),
                },
            }),
        );
        let notes = render_notes_over(24, 80, &base(), state.note_editor().unwrap());
        assert!(notes[0].starts_with("home-row-0-"));
        assert!(notes.join("\n").contains("remember this"));
        assert!(notes.join("\n").contains("Could not save notes"));
        assert!(notes.iter().all(|line| display_width(line) == 80));
        assert_eq!(
            notes_height,
            notes
                .iter()
                .filter(|line| line.contains('│') || line.contains('┌') || line.contains('└'))
                .count()
        );
        for (section, expected) in [
            (NoteSection::Todos, "first"),
            (NoteSection::Decisions, "keep the port boundary"),
        ] {
            let _ = update(
                &mut state,
                AppEvent::Key(AppKey::SelectNoteSection(section)),
            );
            let frame = render_notes_over(24, 80, &base(), state.note_editor().unwrap());
            assert!(frame.join("\n").contains(expected));
        }
    }

    #[test]
    fn environment_overlay_renders_empty_values_errors_and_overflow() {
        let workspace = WorkspaceId::new();
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenEnvironment));
        let empty_environment =
            render_environment_over(24, 30, &base(), state.environment_editor().unwrap());
        let environment_height = empty_environment
            .iter()
            .filter(|line| line.contains('│') || line.contains('┌') || line.contains('└'))
            .count();
        assert!(empty_environment.join("\n").contains("no environment"));
        let entries = (0..9)
            .map(|index| EnvironmentEntry {
                name: format!("KEY_{index}"),
                value: format!("value-{index}"),
            })
            .collect();
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::EnvironmentLoaded {
                target: Target::Root(workspace),
                entries,
            }),
        );
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::EnvironmentError {
                target: Target::Root(workspace),
                error: SafeError {
                    message: SafeMessage::new("Could not save environment"),
                    error_id: "safe-environment".into(),
                },
            }),
        );
        let environment =
            render_environment_over(40, 80, &base(), state.environment_editor().unwrap());
        assert!(environment.join("\n").contains("KEY_0=value-0"));
        assert!(environment.join("\n").contains("1 more"));
        assert!(environment.join("\n").contains("Could not save"));
        assert!(environment.iter().all(|line| display_width(line) == 80));
        assert_eq!(
            environment_height,
            render_environment_over(24, 30, &base(), state.environment_editor().unwrap())
                .iter()
                .filter(|line| line.contains('│') || line.contains('┌') || line.contains('└'))
                .count()
        );
    }
}

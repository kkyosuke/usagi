//! Note scratchpad and environment editor overlays.
//!
//! Both surfaces are pure renderers over controller-owned draft state. Their
//! persistence is deliberately handled by controller effects, keeping the
//! workspace state and settings owners outside presentation.

use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::modal;
use crate::usecase::application::controller::{
    EnvironmentEditor, EnvironmentEntry, NoteEditor, NoteSection,
};
use usagi_core::usecase::env::EnvScope;

const INNER_WIDTH: usize = 62;
const MAX_ROWS: usize = 8;
const NOTES_BODY_HEIGHT: usize = 16;
/// The environment overlay shows two lists (this scope's bindings and the global
/// ones it inherits), an input line, and a footer. The row caps and this height
/// are chosen together so the footer and input line always stay visible.
const ENVIRONMENT_BODY_HEIGHT: usize = 18;
const ENVIRONMENT_MAX_ROWS: usize = 6;
const ENVIRONMENT_MAX_INHERITED_ROWS: usize = 3;

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
    let (caption, scope_hint) = match editor.scope() {
        EnvScope::Workspace => ("this workspace's environment", "Tab: edit global instead"),
        EnvScope::Global => (
            "global environment · every workspace",
            "Tab: edit this workspace instead",
        ),
    };
    let mut lines = vec![modal::caption(caption)];
    if editor.is_loading() {
        // Still reading the persisted values; distinct from a loaded-but-empty
        // environment so the reader knows nothing has refluxed yet.
        lines.push(modal::empty_notice("(loading…)"));
    } else {
        lines.extend(scope_rows(editor));
        lines.extend(inherited_rows(editor));
    }
    if let Some(line) = error_line(editor.error().map(|error| error.message.as_str())) {
        lines.push(String::new());
        lines.push(line);
    }
    lines.push(String::new());
    lines.push(modal::content_line(
        &format!("> {}", editor.draft()),
        INNER_WIDTH,
    ));
    // While a save is in flight the footer reflects it, and the reducer rejects
    // a second Save until the owning port refluxes — no double-submit.
    let footer = if editor.is_saving() {
        "Esc: close   Saving…".to_owned()
    } else {
        format!("Enter: NAME=value / empty: save   {scope_hint}   Esc: close")
    };
    lines.push(modal::footer(&footer));
    modal::fixed_body(lines, ENVIRONMENT_BODY_HEIGHT)
}

/// The bindings the edited scope owns itself.
fn environment_rows(entries: &[EnvironmentEntry]) -> Vec<String> {
    let mut lines: Vec<String> = entries
        .iter()
        .take(ENVIRONMENT_MAX_ROWS)
        .map(|entry| modal::content_line(&format!("{}={}", entry.name, entry.value), INNER_WIDTH))
        .collect();
    if entries.len() > ENVIRONMENT_MAX_ROWS {
        lines.push(modal::caption(&format!(
            "… {} more",
            entries.len() - ENVIRONMENT_MAX_ROWS
        )));
    }
    lines
}

fn scope_rows(editor: &EnvironmentEditor) -> Vec<String> {
    let mut lines = Vec::new();
    if editor.entries().is_empty() {
        lines.push(modal::empty_notice("(no environment variables)"));
    }
    lines.extend(environment_rows(editor.entries()));
    lines
}

/// The global bindings a workspace edit inherits. Shown read-only so a workspace
/// change is made knowing what is already set everywhere; a name this workspace
/// also binds is marked as shadowed rather than hidden, because both values stay
/// stored and the workspace one is what a launch will use.
fn inherited_rows(editor: &EnvironmentEditor) -> Vec<String> {
    if editor.inherited().is_empty() {
        return Vec::new();
    }
    let mut lines = vec![modal::caption("inherited from global")];
    lines.extend(
        editor
            .inherited()
            .iter()
            .take(ENVIRONMENT_MAX_INHERITED_ROWS)
            .map(|entry| {
                let suffix = if editor.shadows(&entry.name) {
                    "   (overridden here)"
                } else {
                    ""
                };
                Style::new().dim().paint(&modal::content_line(
                    &format!("{}={}{suffix}", entry.name, entry.value),
                    INNER_WIDTH,
                ))
            }),
    );
    if editor.inherited().len() > ENVIRONMENT_MAX_INHERITED_ROWS {
        lines.push(modal::caption(&format!(
            "… {} more inherited",
            editor.inherited().len() - ENVIRONMENT_MAX_INHERITED_ROWS
        )));
    }
    lines
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
    use usagi_core::usecase::env::EnvScope;

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
        // Freshly opened: the read is still in flight, shown as loading (not the
        // loaded-but-empty notice).
        let loading_environment =
            render_environment_over(24, 30, &base(), state.environment_editor().unwrap());
        let environment_height = loading_environment
            .iter()
            .filter(|line| line.contains('│') || line.contains('┌') || line.contains('└'))
            .count();
        assert!(loading_environment.join("\n").contains("loading"));
        assert!(!loading_environment.join("\n").contains("no environment"));
        // An empty load resolves the loading state to the empty notice.
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::EnvironmentLoaded {
                scope: EnvScope::Workspace,
                entries: Vec::new(),
                inherited: Vec::new(),
            }),
        );
        let empty_environment =
            render_environment_over(24, 30, &base(), state.environment_editor().unwrap());
        assert!(empty_environment.join("\n").contains("no environment"));
        let entries = (0..7)
            .map(|index| EnvironmentEntry {
                name: format!("KEY_{index}"),
                value: format!("value-{index}"),
            })
            .collect();
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::EnvironmentLoaded {
                scope: EnvScope::Workspace,
                entries,
                inherited: vec![EnvironmentEntry {
                    name: "KEY_0".to_owned(),
                    value: "from-global".to_owned(),
                }],
            }),
        );
        // A save in flight is reflected in the footer and guards re-submission.
        let _ = update(&mut state, AppEvent::Key(AppKey::SaveEnvironment));
        let saving = render_environment_over(40, 80, &base(), state.environment_editor().unwrap());
        assert!(saving.join("\n").contains("Saving"));
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::EnvironmentError {
                scope: EnvScope::Workspace,
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
        // The inherited global binding is listed, and marked as shadowed by the
        // workspace binding of the same name.
        assert!(environment.join("\n").contains("inherited from global"));
        assert!(environment.join("\n").contains("overridden here"));
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

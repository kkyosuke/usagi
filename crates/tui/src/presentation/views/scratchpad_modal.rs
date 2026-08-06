//! Note scratchpad and environment editor overlays.
//!
//! Both surfaces are pure renderers over controller-owned draft state. Their
//! persistence is deliberately handled by controller effects, keeping the
//! workspace state and settings owners outside presentation.

use crate::presentation::theme::Role;
use crate::presentation::widgets::modal;
use crate::usecase::application::controller::{
    EnvironmentEditor, NoteEditor, NoteSection, ROLE_EDITOR_VIEWPORT_LINES, RoleEditor,
    RoleEditorScope,
};
use usagi_core::usecase::env::EnvScope;

const INNER_WIDTH: usize = 62;
const MAX_ROWS: usize = 8;
const NOTES_BODY_HEIGHT: usize = 16;

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
    let scope = match editor.scope() {
        EnvScope::Global => usagi_core::usecase::settings::SettingsScope::Global,
        EnvScope::Workspace => usagi_core::usecase::settings::SettingsScope::Workspace,
    };
    super::config::render_environment_source_over(
        height,
        width,
        base,
        super::config::EnvironmentSource {
            scope,
            value: editor.draft(),
            cursor: editor.cursor(),
            error: editor.error().map(|error| error.message.as_str()),
            save_focused: editor.is_save_focused(),
            ctrl_s_save: true,
        },
    )
}

/// Render the lossless TOML role editor. Validation failures remain inline and
/// leave the complete source available for correction.
#[must_use]
pub fn render_roles_over(
    height: usize,
    width: usize,
    base: &[String],
    editor: &RoleEditor,
) -> Vec<String> {
    let scope = match editor.scope() {
        RoleEditorScope::Global => "global",
        RoleEditorScope::Workspace => "workspace",
    };
    let mut lines = vec![modal::caption(&format!(
        "{scope} roles.toml · versioned TOML"
    ))];
    if editor.is_loading() {
        lines.push(modal::empty_notice("(loading…)"));
    } else {
        lines.extend(
            editor
                .source()
                .lines()
                .skip(editor.scroll_top())
                .take(ROLE_EDITOR_VIEWPORT_LINES)
                .map(|line| modal::content_line(line, INNER_WIDTH)),
        );
    }
    if let Some(line) = error_line(editor.error().map(|error| error.message.as_str())) {
        lines.push(line);
    }
    lines.push(modal::footer(if editor.is_saving() {
        "Saving…"
    } else {
        "Ctrl-S: validate + save   Tab: scope   Esc: close"
    }));
    lines.push(modal::footer("↑/↓: line   PageUp/PageDown: page"));
    modal::render_over(
        height,
        width,
        base,
        "Roles",
        INNER_WIDTH,
        &modal::fixed_body(lines, 18),
    )
}

#[cfg(test)]
mod tests {
    use super::{render_environment_over, render_notes_over, render_roles_over};
    use crate::presentation::widgets::{display_width, strip_ansi};
    use crate::usecase::application::controller::{
        AppEvent, AppKey, AppState, BackendEvent, EnvironmentEntry, NoteSection, RoleEditorScope,
        SafeError, SafeMessage, Target, update,
    };
    use chrono::{TimeZone, Utc};
    use usagi_core::domain::id::{SessionId, WorkspaceId};
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
        let session = SessionId::new();
        let mut state = AppState::home(workspace, vec![session]);
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
                target: Target::Session(session),
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
                target: Target::Session(session),
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
    fn closeup_environment_overlay_matches_workspace_config_editor() {
        let workspace = WorkspaceId::new();
        let mut state = AppState::home(workspace, vec![SessionId::new()]);
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenCloseupOverlay));
        let _ = update(
            &mut state,
            AppEvent::Key(AppKey::SubmitCloseup("env".to_owned())),
        );
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::EnvironmentLoaded {
                scope: EnvScope::Workspace,
                entries: vec![EnvironmentEntry {
                    name: "RUST_LOG".to_owned(),
                    value: "debug".to_owned(),
                }],
                inherited: vec![EnvironmentEntry {
                    name: "GLOBAL_TOKEN".to_owned(),
                    value: "op://vault/item/token".to_owned(),
                }],
            }),
        );
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::EnvironmentError {
                scope: EnvScope::Workspace,
                error: SafeError {
                    message: SafeMessage::new("Could not save environment"),
                    error_id: "safe-closeup-environment".to_owned(),
                },
            }),
        );

        let frame = render_environment_over(40, 80, &base(), state.environment_editor().unwrap())
            .join("\n");
        assert!(frame.contains("workspace env only (global values stay unchanged)"));
        assert!(frame.contains("one NAME=value binding per line"));
        assert!(frame.contains("RUST_LOG=debug"));
        assert!(frame.contains("Could not save environment"));
        assert!(!frame.contains("GLOBAL_TOKEN"));
        assert!(frame.contains("[ Save ]"));
        assert!(frame.contains("Ctrl-S: save"));
        assert!(frame.contains("Enter: newline/save   Tab: switch   Esc: cancel"));
        assert!(frame.contains("\u{1b}[37;48;5;236m"));
        let save_row = frame
            .lines()
            .map(strip_ansi)
            .find(|line| line.contains("[ Save ]"))
            .unwrap();
        assert_eq!(
            display_width(&save_row[..save_row.find("[ Save ]").unwrap()]) + 4,
            40,
            "Closeup Save must be centered in an 80-column frame"
        );
        let rows = frame.lines().map(strip_ansi).collect::<Vec<_>>();
        let save_index = rows
            .iter()
            .position(|line| line.contains("[ Save ]"))
            .unwrap();
        let footer_index = rows
            .iter()
            .position(|line| line.contains("Ctrl-S: save"))
            .unwrap();
        assert_eq!(footer_index, save_index + 2);
    }

    #[test]
    fn overview_environment_overlays_match_config_editors() {
        let workspace = WorkspaceId::new();
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
        let _ = update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("env".to_owned())),
        );
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::EnvironmentLoaded {
                scope: EnvScope::Workspace,
                entries: vec![EnvironmentEntry {
                    name: "RUST_LOG".to_owned(),
                    value: "debug".to_owned(),
                }],
                inherited: Vec::new(),
            }),
        );
        let workspace_frame =
            render_environment_over(40, 80, &base(), state.environment_editor().unwrap())
                .join("\n");
        assert!(workspace_frame.contains("workspace env only (global values stay unchanged)"));
        assert!(workspace_frame.contains("RUST_LOG=debug"));
        assert!(workspace_frame.contains("[ Save ]"));
        assert!(!workspace_frame.contains("Tab: global"));

        let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
        let _ = update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("env global".to_owned())),
        );
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::EnvironmentLoaded {
                scope: EnvScope::Global,
                entries: Vec::new(),
                inherited: Vec::new(),
            }),
        );
        let global_frame =
            render_environment_over(40, 80, &base(), state.environment_editor().unwrap())
                .join("\n");
        assert!(global_frame.contains("global env (inherited by every workspace)"));
        assert!(global_frame.contains("Ctrl-S: save"));
        assert!(global_frame.contains("[ Save ]"));
        assert!(!global_frame.contains("Tab: workspace"));
    }

    #[test]
    fn role_editor_renders_lossless_source_and_inline_validation_error() {
        let mut state = AppState::home(WorkspaceId::new(), Vec::new());
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
        let _ = update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("roles global".into())),
        );
        let loading = render_roles_over(24, 80, &base(), state.role_editor().unwrap()).join("\n");
        assert!(loading.contains("loading"));
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::RolesLoaded {
                scope: RoleEditorScope::Global,
                source: "# comment\nversion = 1\n".into(),
            }),
        );
        let _ = update(&mut state, AppEvent::Key(AppKey::SaveRoles));
        let saving = render_roles_over(24, 80, &base(), state.role_editor().unwrap()).join("\n");
        assert!(saving.contains("Saving"));
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::RolesError {
                scope: RoleEditorScope::Global,
                error: SafeError {
                    message: SafeMessage::new("invalid default role"),
                    error_id: "roles-invalid".into(),
                },
            }),
        );
        let frame = render_roles_over(24, 80, &base(), state.role_editor().unwrap());
        let text = frame.join("\n");
        assert!(text.contains("# comment"));
        assert!(text.contains("invalid default role"));
        assert!(frame.iter().all(|line| display_width(line) == 80));

        let _ = update(&mut state, AppEvent::Key(AppKey::Tab));
        let workspace = render_roles_over(24, 80, &base(), state.role_editor().unwrap()).join("\n");
        assert!(workspace.contains("workspace roles.toml"));

        let source = (0..30)
            .map(|line| format!("line-{line:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::RolesLoaded {
                scope: RoleEditorScope::Workspace,
                source,
            }),
        );
        let tail = render_roles_over(24, 80, &base(), state.role_editor().unwrap()).join("\n");
        assert!(!tail.contains("line-00"));
        assert!(tail.contains("line-29"));
        let _ = update(&mut state, AppEvent::Key(AppKey::PageUp));
        let _ = update(&mut state, AppEvent::Key(AppKey::PageUp));
        let head = render_roles_over(24, 80, &base(), state.role_editor().unwrap()).join("\n");
        assert!(head.contains("line-00"));
        assert!(!head.contains("line-29"));
        assert!(head.contains("PageUp/PageDown"));
    }
}

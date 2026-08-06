//! Note scratchpad and environment editor overlays.
//!
//! Both surfaces are pure renderers over controller-owned draft state. Their
//! persistence is deliberately handled by controller effects, keeping the
//! workspace state and settings owners outside presentation.

use crate::presentation::theme::{Role, Style, editor_surface_style};
use crate::presentation::widgets::{self, modal};
use crate::usecase::application::controller::{
    EnvironmentEditor, EnvironmentEntry, NoteEditor, NoteSection, ROLE_EDITOR_VIEWPORT_LINES,
    RoleEditor, RoleEditorScope,
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
const ENVIRONMENT_INPUT_WIDTH: usize = INNER_WIDTH - 4;

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
    debug_assert!(!editor.is_scope_locked());
    let (caption, scope_hint) = match editor.scope() {
        EnvScope::Workspace => ("this workspace's environment", "Tab: global"),
        EnvScope::Global => ("global environment · every workspace", "Tab: workspace"),
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
    lines.push(environment_draft_row(editor.draft()));
    // While a save is in flight the footer reflects it, and the reducer rejects
    // a second Save until the owning port refluxes — no double-submit.
    let footer = if editor.is_saving() {
        "Esc: close   Saving…".to_owned()
    } else {
        format!("Enter: NAME=value / save   {scope_hint}   Esc: close")
    };
    lines.push(modal::footer(&footer));
    modal::fixed_body(lines, ENVIRONMENT_BODY_HEIGHT)
}

fn environment_draft_row(draft: &str) -> String {
    let content = format!("> {draft}");
    let padding =
        " ".repeat(ENVIRONMENT_INPUT_WIDTH.saturating_sub(widgets::display_width(&content)));
    modal::content_line(
        &editor_surface_style().paint(&format!("{content}{padding}")),
        INNER_WIDTH,
    )
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
    if editor.is_scope_locked() {
        return super::config::render_environment_source_over(
            height,
            width,
            base,
            super::config::EnvironmentSource {
                scope: usagi_core::usecase::settings::SettingsScope::Workspace,
                value: editor.draft(),
                cursor: editor.cursor(),
                error: editor.error().map(|error| error.message.as_str()),
                save_focused: editor.is_save_focused(),
            },
        );
    }
    modal::render_over(
        height,
        width,
        base,
        "Environment",
        INNER_WIDTH,
        &environment_body(editor),
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
    use super::{
        ENVIRONMENT_INPUT_WIDTH, environment_draft_row, render_environment_over, render_notes_over,
        render_roles_over,
    };
    use crate::presentation::widgets::display_width;
    use crate::presentation::widgets::modal::BODY_INDENT_WIDTH;
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
    fn environment_draft_uses_the_shared_fixed_width_surface() {
        for draft in ["", "RUST_LOG=debug"] {
            let row = environment_draft_row(draft);
            assert!(row.contains("\u{1b}[37;48;5;236m"));
            assert_eq!(
                display_width(&row),
                ENVIRONMENT_INPUT_WIDTH + BODY_INDENT_WIDTH
            );
        }
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
    fn environment_overlay_marks_inherited_rows_and_names_the_edited_scope() {
        let workspace = WorkspaceId::new();
        let mut state = AppState::home(workspace, vec![SessionId::new()]);
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenEnvironment));
        let inherited = (0..5)
            .map(|index| EnvironmentEntry {
                name: format!("GLOBAL_{index}"),
                value: format!("g-{index}"),
            })
            .collect();
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::EnvironmentLoaded {
                scope: EnvScope::Workspace,
                entries: vec![EnvironmentEntry {
                    name: "GLOBAL_0".to_owned(),
                    value: "mine".to_owned(),
                }],
                inherited,
            }),
        );
        let frame = render_environment_over(40, 80, &base(), state.environment_editor().unwrap())
            .join("\n");
        // The workspace scope is named, its own binding is listed, and the global
        // set is shown with only the shadowed name marked. The list is capped, so
        // the remainder is counted rather than pushing the footer off the modal.
        assert!(frame.contains("this workspace's environment"));
        assert!(frame.contains("Tab: global"));
        assert!(frame.contains("GLOBAL_0=mine"));
        assert!(frame.contains("GLOBAL_0=g-0   (overridden here)"));
        assert!(frame.contains("GLOBAL_1=g-1"));
        assert!(!frame.contains("GLOBAL_1=g-1   (overridden here)"));
        assert!(frame.contains("2 more inherited"));

        // The global scope names itself and inherits nothing, so no second list.
        let _ = update(&mut state, AppEvent::Key(AppKey::ToggleEnvironmentScope));
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::EnvironmentLoaded {
                scope: EnvScope::Global,
                entries: vec![EnvironmentEntry {
                    name: "GLOBAL_0".to_owned(),
                    value: "g-0".to_owned(),
                }],
                inherited: Vec::new(),
            }),
        );
        let global = render_environment_over(40, 80, &base(), state.environment_editor().unwrap())
            .join("\n");
        assert!(global.contains("global environment"));
        assert!(global.contains("Tab: workspace"));
        assert!(!global.contains("inherited from global"));
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
        assert!(frame.contains("Enter: newline/save   Tab: switch   Esc: cancel"));
        assert!(frame.contains("\u{1b}[37;48;5;236m"));
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

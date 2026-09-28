//! Durable user-decision list and answer editor overlays.

mod context;

use usagi_core::domain::presentation_text::sanitize_presentation_line;
use usagi_core::domain::user_decision::UserDecisionSelectionMode;

use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::{self, modal};
use crate::usecase::application::controller::DecisionOverlayState;

const INNER_WIDTH: usize = 70;
const BODY_HEIGHT: usize = 18;
// Leave room for the persistent footer and for a scroll indicator above and
// below the viewport.  This keeps every decision field reachable even when a
// prompt, option label, or description spans many rows.
const CONTENT_CAPACITY: usize = BODY_HEIGHT - 4;

fn wrapped_content_lines(text: &str, prefix: &str, inner_width: usize) -> Vec<String> {
    let width = inner_width.saturating_sub(modal::BODY_INDENT_WIDTH);
    let continuation = " ".repeat(prefix.len());
    let mut rows = Vec::new();
    for line in text.lines() {
        let mut wrapped = widgets::wrap_to_width(
            &sanitize_presentation_line(line),
            width.saturating_sub(prefix.len()),
        );
        if wrapped.is_empty() {
            wrapped.push(String::new());
        }
        for (index, segment) in wrapped.into_iter().enumerate() {
            let indent = if index == 0 { prefix } else { &continuation };
            rows.push(modal::content_line(
                &format!("{indent}{segment}"),
                inner_width,
            ));
        }
    }
    rows
}

fn wrapped_dim_lines(text: &str, prefix: &str, inner_width: usize) -> Vec<String> {
    wrapped_content_lines(text, prefix, inner_width)
        .into_iter()
        .map(|line| Style::new().dim().paint(&line))
        .collect()
}

fn editor_body(
    editor: &crate::usecase::application::controller::DecisionEditor,
    inner_width: usize,
) -> Vec<String> {
    let decision = editor.decision();
    let multiple = decision.selection_mode == UserDecisionSelectionMode::Multiple;
    let mut rows = editor_intro(editor, inner_width, multiple);
    let mut selected_row = rows.len();
    for (index, option) in decision.options.iter().enumerate() {
        if index == editor.selected_option() {
            selected_row = rows.len();
        }
        let marker = if multiple {
            format!(
                "{} [{}] ",
                modal::selection_marker(
                    index == editor.selected_option() && !editor.input_freeform()
                ),
                if editor.option_checked(&option.id) {
                    "x"
                } else {
                    " "
                }
            )
        } else {
            format!(
                "{} ",
                modal::selection_marker(index == editor.selected_option())
            )
        };
        rows.extend(wrapped_content_lines(&option.label, &marker, inner_width));
        if let Some(description) = &option.description {
            rows.extend(wrapped_dim_lines(description, "     ", inner_width));
        }
    }
    if decision.allow_freeform {
        rows.push(String::new());
        rows.extend(wrapped_content_lines(
            &format!(
                "{}freeform: {}",
                if multiple && editor.input_freeform() {
                    "> "
                } else {
                    ""
                },
                editor.freeform()
            ),
            "",
            inner_width,
        ));
        if editor.follows_freeform() {
            selected_row = rows.len().saturating_sub(1);
        }
    }
    if let Some(error) = editor.error() {
        rows.extend(
            wrapped_content_lines(error.message.as_str(), "", inner_width)
                .into_iter()
                .map(|line| Role::Danger.style().paint(&line)),
        );
        selected_row = rows.len().saturating_sub(1);
    }

    let (start, end) = editor.scroll_offset().map_or_else(
        || modal::list_window(rows.len(), selected_row, CONTENT_CAPACITY),
        |offset| {
            let start = offset.min(rows.len().saturating_sub(CONTENT_CAPACITY));
            let end = start.saturating_add(CONTENT_CAPACITY).min(rows.len());
            (start, end)
        },
    );
    let mut body = modal::scroll_window(&rows, start, end);
    if multiple {
        let count = decision
            .options
            .iter()
            .filter(|option| editor.option_checked(&option.id))
            .count();
        body.push(modal::footer(
            "↑↓: move  Space: check  Enter: submit  Esc: back",
        ));
        body.push(modal::footer(&format!(
            "{count} selected  {}PgUp/PgDn: scroll",
            if decision.allow_freeform {
                "Tab: choices/freeform  "
            } else {
                ""
            }
        )));
    } else {
        body.push(modal::footer(
            "↑↓: select  PgUp/PgDn: scroll  Enter: submit  Esc: back",
        ));
    }
    body
}

fn editor_intro(
    editor: &crate::usecase::application::controller::DecisionEditor,
    inner_width: usize,
    multiple: bool,
) -> Vec<String> {
    let decision = editor.decision();
    let mut rows = Vec::new();
    rows.extend(
        wrapped_content_lines(&decision.title, "", inner_width)
            .into_iter()
            .map(|line| Role::Accent.style().bold().paint(&line)),
    );
    rows.extend(wrapped_content_lines(&decision.prompt, "", inner_width));
    if let Some(deadline) = decision.expires_at {
        rows.push(modal::caption(&format!(
            "expires: {}",
            deadline.format("%Y-%m-%d %H:%M UTC")
        )));
    }
    rows.push(String::new());

    for block in &decision.context {
        rows.extend(context::render(block, inner_width, editor.context_column()));
        rows.push(String::new());
    }
    if multiple {
        let count = decision
            .options
            .iter()
            .filter(|option| editor.option_checked(&option.id))
            .count();
        rows.extend(wrapped_content_lines(
            &format!("Select one or more ({count} selected)"),
            "",
            inner_width,
        ));
    }
    rows
}

fn list_body(
    overlay: &DecisionOverlayState,
    decisions: &[usagi_core::domain::user_decision::UserDecision],
    inner_width: usize,
) -> Vec<String> {
    let mut rows = vec![modal::caption("Pending decisions for this workspace")];
    if decisions.is_empty() {
        rows.push(modal::empty_notice("(none)"));
    }
    let mut selected_row = rows.len();
    for (index, decision) in decisions.iter().enumerate() {
        if index == overlay.selected() {
            selected_row = rows.len();
        }
        let marker = format!("{} ", modal::selection_marker(index == overlay.selected()));
        let session = decision
            .owner
            .session_id
            .as_ref()
            .map_or_else(|| "workspace root".to_owned(), ToString::to_string);
        rows.extend(wrapped_content_lines(
            &format!("{session}: {}", decision.title),
            &marker,
            inner_width,
        ));
    }
    let (start, end) = modal::list_window(rows.len(), selected_row, CONTENT_CAPACITY);
    let mut body = modal::scroll_window(&rows, start, end);
    body.push(String::new());
    body.push(modal::footer("↑↓: select   Enter: open   Esc: close"));
    body
}

/// Render either the workspace pending list or the selected decision editor.
#[must_use]
pub fn render_over(
    height: usize,
    width: usize,
    base: &[String],
    overlay: &DecisionOverlayState,
    decisions: &[usagi_core::domain::user_decision::UserDecision],
) -> Vec<String> {
    let inner_width = modal::modal_inner_width(width, INNER_WIDTH);
    let (title, body) = if let Some(editor) = overlay.editor() {
        ("User decision", editor_body(editor, inner_width))
    } else {
        (
            "Pending decisions",
            list_body(overlay, decisions, inner_width),
        )
    };
    modal::render_over(
        height,
        width,
        base,
        title,
        inner_width,
        &modal::fixed_body(body, BODY_HEIGHT),
    )
}

#[cfg(test)]
mod tests {
    #![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract
    use super::*;
    use crate::usecase::application::controller::{
        AppEvent, AppKey, AppState, BackendEvent, SafeError, SafeMessage, update,
    };
    use usagi_core::domain::agent::CallerRef;
    use usagi_core::domain::id::{AgentId, OperationId, SessionId, UserDecisionId, WorkspaceId};
    use usagi_core::domain::user_decision::{
        UserDecision, UserDecisionOption, UserDecisionOwner, UserDecisionStatus,
    };

    fn decision(workspace: WorkspaceId, session_id: Option<SessionId>) -> UserDecision {
        UserDecision {
            decision_id: UserDecisionId::new(),
            owner: UserDecisionOwner {
                workspace_id: workspace,
                session_id,
                caller: CallerRef {
                    session_id,
                    agent_id: AgentId::new(),
                },
                run_id: OperationId::new(),
            },
            title: "Choose".to_owned(),
            prompt: "Pick one\n\ncarefully".to_owned(),
            options: vec![UserDecisionOption {
                id: "safe".to_owned(),
                label: "Safe".to_owned(),
                description: Some("keep state".to_owned()),
            }],
            allow_freeform: true,
            selection_mode: usagi_core::domain::user_decision::UserDecisionSelectionMode::Single,
            context: Vec::new(),
            expires_at: Some(chrono::Utc::now()),
            idempotency_key: None,
            status: UserDecisionStatus::Pending,
            answer: None,
            created_at: chrono::Utc::now(),
            resolved_at: None,
        }
    }

    #[test]
    fn renders_empty_list_root_and_session_rows_and_the_full_editor() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenDecisions));
        let empty = render_over(
            24,
            80,
            &["base".to_owned()],
            state.decision_overlay().unwrap(),
            &[],
        );
        assert!(empty.join("\n").contains("(none)"));

        let mut root = decision(workspace, None);
        root.allow_freeform = false;
        let mut scoped = decision(workspace, Some(session));
        scoped.prompt = (0..CONTENT_CAPACITY)
            .map(|index| format!("context line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        scoped.prompt.insert(0, '\n');
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Decisions {
                workspace,
                decisions: vec![root.clone(), scoped.clone()],
            }),
        );
        let list = render_over(
            24,
            80,
            &[],
            state.decision_overlay().unwrap(),
            &[root.clone(), scoped.clone()],
        );
        assert!(list.join("\n").contains("workspace root"));

        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let fixed_options = render_over(
            24,
            80,
            &[],
            state.decision_overlay().unwrap(),
            &[root, scoped.clone()],
        );
        assert!(!fixed_options.join("\n").contains("freeform:"));
        let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
        let _ = update(&mut state, AppEvent::Key(AppKey::DecisionNext));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let _ = update(&mut state, AppEvent::Key(AppKey::PageDown));
        let scrolled = render_over(
            24,
            80,
            &[],
            state.decision_overlay().unwrap(),
            &[scoped.clone()],
        );
        assert!(scrolled.join("\n").contains("context line"));
        let _ = update(&mut state, AppEvent::Key(AppKey::PageUp));
        let _ = update(
            &mut state,
            AppEvent::Key(AppKey::SetDecisionFreeform("custom".to_owned())),
        );
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::DecisionError {
                workspace,
                decision_id: scoped.decision_id,
                error: SafeError {
                    message: SafeMessage::new("retry"),
                    error_id: "decision".to_owned(),
                },
            }),
        );
        let editor = render_over(24, 80, &[], state.decision_overlay().unwrap(), &[scoped]);
        let text = editor.join("\n");
        assert!(text.contains("freeform: custom"));
        assert!(text.contains("expires:"));
        assert!(text.contains("retry"));
    }
    #[test]
    fn context_tables_wrap_or_stack_and_diagrams_preserve_spacing_when_panned() {
        use usagi_core::domain::user_decision::UserDecisionContext;
        let table = UserDecisionContext::Table {
            title: "Costs".into(),
            columns: vec!["Plan".into(), "Details".into()],
            rows: vec![
                vec![
                    "日本語".into(),
                    "long description that wraps\nnext line".into(),
                ],
                vec![String::new(), "value".into()],
            ],
        };
        let wide = context::render(&table, 40, 0).join("\n");
        assert!(wide.contains("│"));
        assert!(wide.contains("日本語"));
        assert!(wide.contains("next line"));
        let narrow = context::render(&table, 16, 0).join("\n");
        assert!(narrow.contains("Plan:"));
        assert!(narrow.contains("Details:"));
        for width in [0, 1, 16, 40] {
            for line in context::render(&table, width, 0) {
                assert!(widgets::display_width(&line) <= width);
            }
        }
        let diagram = UserDecisionContext::Diagram {
            title: "Flow".into(),
            text: "A -> B\n     |\n日本語 -> C\n\u{1b}[31m".into(),
        };
        let rows = context::render(&diagram, 30, 0).join("\n");
        assert!(rows.contains("     |"));
        assert!(!rows.contains('\u{1b}'));
        let panned = context::render(&diagram, 30, 6).join("\n");
        assert!(panned.contains(" -> C"));
        let aligned = UserDecisionContext::Diagram {
            title: "Aligned".into(),
            text: "       界|\n         |".into(),
        };
        let aligned = context::render(&aligned, 30, 8);
        assert_eq!(aligned[1].find('|'), aligned[2].find('|'));
        let past_end = context::render(&diagram, 30, 100).join("\n");
        assert!(!past_end.contains("A -> B"));
    }

    #[test]
    fn rich_editor_shows_context_before_choices_and_explicit_multiselect_controls() {
        use usagi_core::domain::user_decision::UserDecisionContext;
        let workspace = WorkspaceId::new();
        let mut request = decision(workspace, None);
        request.expires_at = None;
        request.selection_mode = UserDecisionSelectionMode::Multiple;
        request.context = vec![UserDecisionContext::Diagram {
            title: "Architecture".into(),
            text: "A -> B".into(),
        }];
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Decisions {
                workspace,
                decisions: vec![request.clone()],
            }),
        );
        let render = |state: &AppState| {
            render_over(
                30,
                90,
                &[],
                state.decision_overlay().unwrap(),
                &[request.clone()],
            )
            .join("\n")
        };
        assert!(render(&state).contains("Architecture"));
        assert!(render(&state).contains("[ ]"));
        let _ = update(&mut state, AppEvent::Key(AppKey::Char(' ')));
        assert!(render(&state).contains("[x]"));
        assert!(render(&state).contains("1 selected"));
        let _ = update(&mut state, AppEvent::Key(AppKey::Tab));
        assert!(render(&state).contains("> freeform:"));
        let mut state = AppState::home(workspace, Vec::new());
        request.allow_freeform = false;
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Decisions {
                workspace,
                decisions: vec![request],
            }),
        );
        assert!(
            editor_body(state.decision_overlay().unwrap().editor().unwrap(), 70)
                .join("\n")
                .contains("Space: check")
        );
    }
    #[test]
    fn empty_multiselect_submission_reveals_error_below_long_context() {
        use usagi_core::domain::user_decision::UserDecisionContext;
        let workspace = WorkspaceId::new();
        let mut request = decision(workspace, None);
        request.expires_at = None;
        request.allow_freeform = false;
        request.selection_mode = UserDecisionSelectionMode::Multiple;
        request.context = vec![UserDecisionContext::Diagram {
            title: "Long diagram".into(),
            text: "A -> B\n".repeat(40),
        }];
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Decisions {
                workspace,
                decisions: vec![request],
            }),
        );
        assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
        let editor = state.decision_overlay().unwrap().editor().unwrap();
        assert_eq!(editor.scroll_offset(), None);
        let body = editor_body(editor, 70).join("\n");
        assert!(body.contains("select a valid answer"));
        assert!(body.contains("PgUp/PgDn"));
        assert!(body.contains("0 selected"));
    }
    #[test]
    fn multiselect_navigation_returns_to_choices_after_validation_error() {
        let workspace = WorkspaceId::new();
        let mut request = decision(workspace, None);
        request.expires_at = None;
        request.allow_freeform = false;
        request.selection_mode = UserDecisionSelectionMode::Multiple;
        request.options = (0..32)
            .map(|index| UserDecisionOption {
                id: format!("choice-{index}"),
                label: format!("Choice number {index}"),
                description: None,
            })
            .collect();
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Decisions {
                workspace,
                decisions: vec![request],
            }),
        );
        assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let editor = state.decision_overlay().unwrap().editor().unwrap();
        assert!(editor.error().is_none());
        let body = editor_body(editor, 70).join("\n");
        assert!(body.contains("Choice number 1"));
        assert!(!body.contains("Choice number 31"));
        assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
        let _ = update(&mut state, AppEvent::Key(AppKey::Up));
        let editor = state.decision_overlay().unwrap().editor().unwrap();
        assert!(editor.error().is_none());
        assert!(
            editor_body(editor, 70)
                .join("\n")
                .contains("Choice number 0")
        );
    }
}

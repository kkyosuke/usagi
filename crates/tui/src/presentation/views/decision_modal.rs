//! Durable user-decision list and answer editor overlays.

mod composition;
mod context;

use usagi_core::domain::presentation_text::sanitize_presentation_line;
use usagi_core::domain::user_decision::UserDecisionSelectionMode;

use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::{self, modal};
use crate::usecase::application::controller::DecisionOverlayState;
use std::collections::BTreeMap;
use usagi_core::domain::id::SessionId;

// The modal grows with the terminal so comparison tables, diagrams, and long
// option lists stay readable, while the minimum keeps the historical 70x18
// frame on an ordinary 80x24 terminal.
const MIN_INNER_WIDTH: usize = 70;
const MAX_INNER_WIDTH: usize = 120;
const MIN_BODY_HEIGHT: usize = 18;
const MAX_BODY_HEIGHT: usize = 40;
// Leave room for the persistent footer and for a scroll indicator above and
// below the viewport.  This keeps every decision field reachable even when a
// prompt, option label, or description spans many rows.
const CHROME_ROWS: usize = 4;
#[cfg(test)]
const CONTENT_CAPACITY: usize = MIN_BODY_HEIGHT - CHROME_ROWS;

/// Four fifths of `available`, bounded to the modal's readable range.
const fn scaled(available: usize, min: usize, max: usize) -> usize {
    let desired = available.saturating_mul(4) / 5;
    if desired < min {
        min
    } else if desired > max {
        max
    } else {
        desired
    }
}

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

fn editor_rows(
    editor: &crate::usecase::application::controller::DecisionEditor,
    inner_width: usize,
    capacity: usize,
) -> Vec<String> {
    if let Some(answer) = editor.confirmation() {
        return composition::confirmation_body(editor, answer, inner_width, capacity);
    }
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
                    index == editor.selected_option()
                        && !editor.input_freeform()
                        && !editor.input_comment()
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
                modal::selection_marker(
                    index == editor.selected_option()
                        && !editor.input_comment()
                        && (!decision.allow_comment || !editor.input_freeform())
                )
            )
        };
        let label = if decision
            .recommendation
            .as_ref()
            .is_some_and(|rec| rec.option_ids.contains(&option.id))
        {
            format!("{} [recommended]", option.label)
        } else {
            option.label.clone()
        };
        rows.extend(wrapped_content_lines(&label, &marker, inner_width));
        if let Some(description) = &option.description {
            rows.extend(wrapped_dim_lines(description, "     ", inner_width));
        }
        for (prefix, points) in [("  Pro: ", &option.pros), ("  Con: ", &option.cons)] {
            for point in points {
                rows.extend(wrapped_content_lines(point, prefix, inner_width));
            }
        }
    }
    if decision.allow_comment {
        rows.extend(composition::comment_rows(editor, inner_width));
        if editor.input_comment() {
            selected_row = rows.len().saturating_sub(1);
        }
    }
    if decision.allow_freeform {
        rows.push(String::new());
        rows.extend(wrapped_content_lines(
            &format!(
                "{}freeform: {}",
                if (multiple || decision.allow_comment) && editor.input_freeform() {
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
        || modal::list_window(rows.len(), selected_row, capacity),
        |offset| {
            let start = offset.min(rows.len().saturating_sub(capacity));
            let end = start.saturating_add(capacity).min(rows.len());
            (start, end)
        },
    );
    let mut body = modal::scroll_window(&rows, start, end);
    body.extend(editor_footer(editor, multiple));
    body
}

fn editor_footer(
    editor: &crate::usecase::application::controller::DecisionEditor,
    multiple: bool,
) -> Vec<String> {
    let decision = editor.decision();
    let mut body = Vec::new();
    if decision.allow_comment || decision.require_confirmation {
        return composition::editor_footer(editor, multiple);
    }
    if multiple {
        let count = decision
            .options
            .iter()
            .filter(|option| editor.option_checked(&option.id))
            .count();
        body.push(modal::footer(
            "↑↓: move  Space: check  Enter: submit  Esc: back",
        ));
        let (min, max) = decision.selection_bounds();
        body.push(modal::footer(&format!(
            "{count} selected ({min}-{max})  {}PgUp/PgDn: scroll",
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

    if let Some(recommendation) = &decision.recommendation {
        let labels = decision
            .options
            .iter()
            .filter(|option| recommendation.option_ids.contains(&option.id))
            .map(|option| option.label.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        rows.extend(wrapped_content_lines(
            &format!("Agent recommendation: {labels}"),
            "",
            inner_width,
        ));
        rows.extend(wrapped_content_lines(
            &recommendation.reason,
            "",
            inner_width,
        ));
        rows.push(String::new());
    }
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
        let (min, max) = decision.selection_bounds();
        rows.extend(wrapped_content_lines(
            &format!("Choose {min}-{max} options ({count} selected)"),
            "",
            inner_width,
        ));
    }
    rows
}

fn list_body(
    overlay: &DecisionOverlayState,
    decisions: &[usagi_core::domain::user_decision::UserDecision],
    session_names: &BTreeMap<SessionId, String>,
    inner_width: usize,
    capacity: usize,
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
        rows.extend(wrapped_content_lines(
            &format!(
                "{}: {}",
                owner_label(decision, session_names),
                decision.title
            ),
            &marker,
            inner_width,
        ));
    }
    let (start, end) = modal::list_window(rows.len(), selected_row, capacity);
    let mut body = modal::scroll_window(&rows, start, end);
    body.push(String::new());
    body.push(modal::footer("↑↓: select   Enter: open   Esc: close"));
    body
}

/// Human-readable owner of a decision: the session name, `workspace root`, or
/// a short ID when the session is not (yet) in the projected session list.
#[must_use]
pub fn owner_label(
    decision: &usagi_core::domain::user_decision::UserDecision,
    session_names: &BTreeMap<SessionId, String>,
) -> String {
    decision.owner.session_id.map_or_else(
        || "workspace root".to_owned(),
        |session| {
            session_names.get(&session).cloned().unwrap_or_else(|| {
                let id = session.to_string();
                format!("session {}", id.get(..8).unwrap_or(&id))
            })
        },
    )
}

/// Render either the workspace pending list or the selected decision editor.
#[must_use]
pub fn render_over(
    height: usize,
    width: usize,
    base: &[String],
    overlay: &DecisionOverlayState,
    decisions: &[usagi_core::domain::user_decision::UserDecision],
    session_names: &BTreeMap<SessionId, String>,
) -> Vec<String> {
    let inner_width =
        modal::modal_inner_width(width, scaled(width, MIN_INNER_WIDTH, MAX_INNER_WIDTH));
    let body_height = modal::reserved_body_height(
        height,
        width,
        scaled(height, MIN_BODY_HEIGHT, MAX_BODY_HEIGHT),
    );
    let capacity = body_height.saturating_sub(CHROME_ROWS).max(1);
    let (title, body) = if let Some(editor) = overlay.editor() {
        ("User decision", editor_rows(editor, inner_width, capacity))
    } else {
        (
            "Pending decisions",
            list_body(overlay, decisions, session_names, inner_width, capacity),
        )
    };
    modal::render_over(
        height,
        width,
        base,
        title,
        inner_width,
        &modal::fixed_body(body, body_height),
    )
}

#[cfg(test)]
mod tests {
    #![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract
    use super::*;

    fn editor_body(
        editor: &crate::usecase::application::controller::DecisionEditor,
        inner_width: usize,
    ) -> Vec<String> {
        editor_rows(editor, inner_width, CONTENT_CAPACITY)
    }
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
                pros: Vec::new(),
                cons: Vec::new(),
                id: "safe".to_owned(),
                label: "Safe".to_owned(),
                description: Some("keep state".to_owned()),
            }],
            allow_freeform: true,
            allow_comment: false,
            require_confirmation: false,
            recommendation: None,
            selection_limits: None,
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
            &BTreeMap::new(),
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
        let names = BTreeMap::from([(session, "issue-42".to_owned())]);
        let list = render_over(
            24,
            80,
            &[],
            state.decision_overlay().unwrap(),
            &[root.clone(), scoped.clone()],
            &names,
        );
        let list = list.join("\n");
        assert!(list.contains("workspace root"));
        assert!(list.contains("issue-42: Choose"));
        assert!(!list.contains(&session.to_string()));

        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let fixed_options = render_over(
            24,
            80,
            &[],
            state.decision_overlay().unwrap(),
            &[root, scoped.clone()],
            &BTreeMap::new(),
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
            &BTreeMap::new(),
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
        let editor = render_over(
            24,
            80,
            &[],
            state.decision_overlay().unwrap(),
            &[scoped],
            &BTreeMap::new(),
        );
        let text = editor.join("\n");
        assert!(text.contains("freeform: custom"));
        assert!(text.contains("expires:"));
        assert!(text.contains("retry"));
    }
    #[test]
    fn owner_label_falls_back_to_a_short_session_id() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let names = BTreeMap::new();
        let label = owner_label(&decision(workspace, Some(session)), &names);
        let id = session.to_string();
        assert_eq!(label, format!("session {}", &id[..8]));
        assert_eq!(
            owner_label(&decision(workspace, None), &names),
            "workspace root"
        );
    }

    #[test]
    fn modal_scales_with_the_terminal_within_its_bounds() {
        assert_eq!(
            scaled(80, MIN_INNER_WIDTH, MAX_INNER_WIDTH),
            MIN_INNER_WIDTH
        );
        assert_eq!(scaled(125, MIN_INNER_WIDTH, MAX_INNER_WIDTH), 100);
        assert_eq!(
            scaled(300, MIN_INNER_WIDTH, MAX_INNER_WIDTH),
            MAX_INNER_WIDTH
        );

        let workspace = WorkspaceId::new();
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenDecisions));
        let rendered = |height, width| {
            render_over(
                height,
                width,
                &[],
                state.decision_overlay().unwrap(),
                &[],
                &BTreeMap::new(),
            )
        };
        let border_width = |lines: &[String]| {
            lines
                .iter()
                .map(|line| widgets::display_width(line.trim_end()))
                .max()
                .unwrap_or_default()
        };
        let border_rows =
            |lines: &[String]| lines.iter().filter(|line| !line.trim().is_empty()).count();
        let small = rendered(24, 80);
        let large = rendered(50, 160);
        assert!(border_width(&large) > border_width(&small));
        assert!(border_rows(&large) > border_rows(&small));
        // A short terminal shrinks the body instead of clipping the footer.
        assert!(rendered(12, 80).join("\n").contains("Esc: close"));
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
                &BTreeMap::new(),
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
                pros: Vec::new(),
                cons: Vec::new(),
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
    #[test]
    fn decision_guidance_shows_reasons_without_preselecting_and_enforces_limits() {
        use usagi_core::domain::user_decision::{
            UserDecisionAnswer, UserDecisionRecommendation, UserDecisionSelectionLimits,
        };
        let workspace = WorkspaceId::new();
        let mut request = decision(workspace, None);
        request.expires_at = None;
        request.selection_mode = UserDecisionSelectionMode::Multiple;
        request.selection_limits = Some(UserDecisionSelectionLimits { min: 2, max: 2 });
        request.options = ["A", "B", "C"]
            .map(|id| UserDecisionOption {
                pros: Vec::new(),
                cons: Vec::new(),
                id: id.into(),
                label: id.into(),
                description: None,
            })
            .to_vec();
        request.recommendation = Some(UserDecisionRecommendation {
            option_ids: vec!["B".into(), "C".into()],
            reason: "Lower effort".into(),
        });
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Decisions {
                workspace,
                decisions: vec![request],
            }),
        );
        let body = |state: &AppState| {
            editor_body(state.decision_overlay().unwrap().editor().unwrap(), 70).join("\n")
        };
        assert!(body(&state).contains("Lower effort"));
        assert!(body(&state).contains("B [recommended]"));
        assert!(body(&state).contains("0 selected (2-2)"));
        assert!(!body(&state).contains("[x]"));
        let _ = update(&mut state, AppEvent::Key(AppKey::Char(' ')));
        assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
        assert!(body(&state).contains("Choose 2-2 options (1 selected)"));
        for key in [
            AppKey::Down,
            AppKey::Char(' '),
            AppKey::Down,
            AppKey::Char(' '),
        ] {
            assert!(update(&mut state, AppEvent::Key(key)).is_empty());
        }
        let editor = state.decision_overlay().unwrap().editor().unwrap();
        assert!(!editor.option_checked("C"));
        assert!(body(&state).contains("uncheck one"));
        assert!(body(&state).contains("2 selected (2-2)"));
        assert!(
            matches!(update(&mut state, AppEvent::Key(AppKey::Enter)).as_slice(),
            [crate::usecase::application::controller::Effect::ResolveDecision { answer: UserDecisionAnswer::Options { option_ids, .. }, .. }] if option_ids == &["A", "B"])
        );
        assert!(
            state
                .decision_overlay()
                .unwrap()
                .editor()
                .unwrap()
                .error()
                .is_none()
        );
        for key in [
            AppKey::Up,
            AppKey::Char(' '),
            AppKey::Down,
            AppKey::Char(' '),
        ] {
            let _ = update(&mut state, AppEvent::Key(key));
        }
        assert!(
            matches!(update(&mut state, AppEvent::Key(AppKey::Enter)).as_slice(),
            [crate::usecase::application::controller::Effect::ResolveDecision { answer: UserDecisionAnswer::Options { option_ids, .. }, .. }] if option_ids == &["A", "C"])
        );
        let _ = update(&mut state, AppEvent::Key(AppKey::Tab));
        let _ = update(
            &mut state,
            AppEvent::Key(AppKey::Paste("Alternative".into())),
        );
        assert!(
            matches!(update(&mut state, AppEvent::Key(AppKey::Enter)).as_slice(),
            [crate::usecase::application::controller::Effect::ResolveDecision { answer: UserDecisionAnswer::Freeform { text }, .. }] if text == "Alternative")
        );
    }
    #[test]
    fn decision_tradeoffs_wrap_scroll_and_keep_answers_bound_to_option_ids() {
        use usagi_core::domain::user_decision::UserDecisionAnswer;
        for mode in [
            UserDecisionSelectionMode::Single,
            UserDecisionSelectionMode::Multiple,
        ] {
            let workspace = WorkspaceId::new();
            let mut request = decision(workspace, None);
            request.expires_at = None;
            request.selection_mode = mode;
            request.options[0].pros = vec![
                "Useful benefit".into(),
                "長いメリットを確認する ".repeat(12),
            ];
            request.options[0].cons = vec!["TRADEOFF_END".into()];
            let mut state = AppState::home(workspace, Vec::new());
            let _ = update(
                &mut state,
                AppEvent::Backend(BackendEvent::Decisions {
                    workspace,
                    decisions: vec![request],
                }),
            );
            let mut seen = String::new();
            for _ in 0..12 {
                let editor = state.decision_overlay().unwrap().editor().unwrap();
                seen.push_str(&editor_body(editor, 32).join("\n"));
                let _ = update(&mut state, AppEvent::Key(AppKey::PageDown));
            }
            assert!(seen.contains("Pro: Useful benefit"));
            assert!(seen.contains("Con: TRADEOFF_END"));
            if mode == UserDecisionSelectionMode::Multiple {
                let _ = update(&mut state, AppEvent::Key(AppKey::Char(' ')));
            }
            let effects = update(&mut state, AppEvent::Key(AppKey::Enter));
            let expected = if mode == UserDecisionSelectionMode::Single {
                UserDecisionAnswer::Option {
                    comment: None,
                    option_id: "safe".into(),
                }
            } else {
                UserDecisionAnswer::Options {
                    comment: None,
                    option_ids: vec!["safe".into()],
                }
            };
            assert!(
                matches!(effects.as_slice(), [crate::usecase::application::controller::Effect::ResolveDecision { answer, .. }] if answer == &expected)
            );
        }
        for width in [0, 1, 16, 32, 70] {
            for line in wrapped_content_lines("注意点を確認する長い文\nnext line", "  Con: ", width)
            {
                assert!(widgets::display_width(&line) <= width);
            }
        }
    }
    #[test]
    fn decision_comment_review_preserves_edits_and_retries_without_early_delivery() {
        use usagi_core::domain::user_decision::UserDecisionAnswer;
        for mode in [
            UserDecisionSelectionMode::Single,
            UserDecisionSelectionMode::Multiple,
        ] {
            let workspace = WorkspaceId::new();
            let mut request = decision(workspace, None);
            request.expires_at = None;
            request.allow_comment = true;
            request.require_confirmation = true;
            request.selection_mode = mode;
            let id = request.decision_id;
            let mut state = AppState::home(workspace, Vec::new());
            let snapshot = AppEvent::Backend(BackendEvent::Decisions {
                workspace,
                decisions: vec![request],
            });
            let _ = update(&mut state, snapshot.clone());
            if mode == UserDecisionSelectionMode::Multiple {
                assert!(update(&mut state, AppEvent::Key(AppKey::Char(' '))).is_empty());
            }
            for key in [
                AppKey::Tab,
                AppKey::Paste("Only staging?".into()),
                AppKey::Backspace,
                AppKey::Char('!'),
            ] {
                assert!(update(&mut state, AppEvent::Key(key)).is_empty());
            }
            let body = |state: &AppState| {
                editor_body(state.decision_overlay().unwrap().editor().unwrap(), 70).join("\n")
            };
            assert!(body(&state).contains("comment (optional): Only staging!"));
            assert!(body(&state).contains("Enter: review"));
            assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
            assert!(body(&state).contains("Review answer"));
            assert!(body(&state).contains("Choice: Safe [safe]"));
            assert!(body(&state).contains("Comment: Only staging!"));
            for key in [
                AppKey::Char('x'),
                AppKey::PageDown,
                AppKey::PageUp,
                AppKey::Escape,
            ] {
                assert!(update(&mut state, AppEvent::Key(key)).is_empty());
            }
            assert!(!body(&state).contains("Review answer"));
            assert!(body(&state).contains("Only staging!"));
            for key in [AppKey::Backspace, AppKey::Char('.'), AppKey::Enter] {
                assert!(update(&mut state, AppEvent::Key(key)).is_empty());
            }
            let _ = update(&mut state, snapshot);
            assert!(body(&state).contains("Comment: Only staging."));
            let _ = update(
                &mut state,
                AppEvent::Backend(BackendEvent::DecisionError {
                    workspace,
                    decision_id: id,
                    error: SafeError {
                        message: SafeMessage::new("Try again"),
                        error_id: "retry".into(),
                    },
                }),
            );
            assert!(body(&state).contains("Try again"));
            let effects = update(&mut state, AppEvent::Key(AppKey::Enter));
            let expected = if mode == UserDecisionSelectionMode::Single {
                UserDecisionAnswer::Option {
                    option_id: "safe".into(),
                    comment: Some("Only staging.".into()),
                }
            } else {
                UserDecisionAnswer::Options {
                    option_ids: vec!["safe".into()],
                    comment: Some("Only staging.".into()),
                }
            };
            assert!(
                matches!(effects.as_slice(), [crate::usecase::application::controller::Effect::ResolveDecision {answer, ..}] if answer == &expected)
            );
            assert!(!body(&state).contains("Try again"));
        }
    }

    #[test]
    fn decision_freeform_review_and_comment_focus_are_independent() {
        use usagi_core::domain::user_decision::UserDecisionAnswer;
        for allow_comment in [false, true] {
            for mode in [
                UserDecisionSelectionMode::Single,
                UserDecisionSelectionMode::Multiple,
            ] {
                let workspace = WorkspaceId::new();
                let mut request = decision(workspace, None);
                request.expires_at = None;
                request.allow_comment = allow_comment;
                request.require_confirmation = true;
                request.selection_mode = mode;
                let mut state = AppState::home(workspace, Vec::new());
                let _ = update(
                    &mut state,
                    AppEvent::Backend(BackendEvent::Decisions {
                        workspace,
                        decisions: vec![request],
                    }),
                );
                if allow_comment {
                    for key in [
                        AppKey::Tab,
                        AppKey::Paste("Choice-only note".into()),
                        AppKey::Tab,
                        AppKey::Tab,
                        AppKey::Tab,
                    ] {
                        assert!(update(&mut state, AppEvent::Key(key)).is_empty());
                    }
                }
                let text = "Alternative\n".repeat(25);
                let _ = update(
                    &mut state,
                    AppEvent::Key(AppKey::SetDecisionFreeform(text.clone())),
                );
                let body =
                    editor_body(state.decision_overlay().unwrap().editor().unwrap(), 70).join("\n");
                assert!(body.contains("Enter: review"));
                assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
                let body =
                    editor_body(state.decision_overlay().unwrap().editor().unwrap(), 70).join("\n");
                assert!(body.contains("Answer: Alternative"));
                assert!(!body.contains("Choice-only note"));
                let _ = update(&mut state, AppEvent::Key(AppKey::PageDown));
                let effects = update(&mut state, AppEvent::Key(AppKey::SubmitDecision));
                assert!(
                    matches!(effects.as_slice(), [crate::usecase::application::controller::Effect::ResolveDecision {answer: UserDecisionAnswer::Freeform {text: answer}, ..}] if answer == text.trim())
                );
            }
        }
    }

    #[test]
    fn decision_comment_without_confirmation_is_optional_and_validated() {
        for mode in [
            UserDecisionSelectionMode::Single,
            UserDecisionSelectionMode::Multiple,
        ] {
            let workspace = WorkspaceId::new();
            let mut request = decision(workspace, None);
            request.expires_at = None;
            request.allow_freeform = false;
            request.allow_comment = true;
            request.selection_mode = mode;
            let mut state = AppState::home(workspace, Vec::new());
            let _ = update(
                &mut state,
                AppEvent::Backend(BackendEvent::Decisions {
                    workspace,
                    decisions: vec![request],
                }),
            );
            if mode == UserDecisionSelectionMode::Multiple {
                let _ = update(&mut state, AppEvent::Key(AppKey::Char(' ')));
            }
            assert_eq!(update(&mut state, AppEvent::Key(AppKey::Enter)).len(), 1);
            for key in [AppKey::Tab, AppKey::Char(' '), AppKey::Tab, AppKey::Tab] {
                assert!(update(&mut state, AppEvent::Key(key)).is_empty());
            }
            let body =
                editor_body(state.decision_overlay().unwrap().editor().unwrap(), 70).join("\n");
            assert!(body.contains("Tab: choices/comment"));
            assert!(body.contains("Enter: submit"));
            assert_eq!(update(&mut state, AppEvent::Key(AppKey::Enter)).len(), 1);
            let _ = update(&mut state, AppEvent::Key(AppKey::Paste("x".repeat(2049))));
            assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
            assert!(
                state
                    .decision_overlay()
                    .unwrap()
                    .editor()
                    .unwrap()
                    .error()
                    .is_some()
            );
        }
    }
}

//! Durable user-decision list and answer editor overlays.

mod composition;
mod context;
mod layout;

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
    layout::wrapped_rows(
        text,
        prefix,
        inner_width.saturating_sub(modal::BODY_INDENT_WIDTH),
    )
    .into_iter()
    .map(|line| modal::content_line(&line, inner_width))
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
    let mut focus = rows.len()..rows.len();
    let mut anchor = rows.len();
    let border_rows = layout::border_rows(inner_width);
    let content_width = layout::content_width(inner_width);
    for (index, option) in decision.options.iter().enumerate() {
        let focused = index == editor.selected_option()
            && !editor.input_freeform()
            && !editor.input_comment();
        let marker = if multiple {
            format!(
                "{} {} ",
                modal::selection_marker(focused),
                if editor.option_checked(&option.id) {
                    Role::Success.style().bold().paint("[x]")
                } else {
                    "[ ]".to_owned()
                }
            )
        } else {
            format!("{} ", modal::selection_marker(focused))
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
        let mut card = layout::heading_rows(&label, &marker, content_width);
        let indent = " ".repeat(widgets::display_width(&marker));
        if let Some(description) = &option.description {
            card.extend(layout::wrapped_rows(description, &indent, content_width));
        }
        for (prefix, points) in [("Pro: ", &option.pros), ("Con: ", &option.cons)] {
            for point in points {
                card.extend(layout::wrapped_rows(
                    point,
                    &format!("{indent}{prefix}"),
                    content_width,
                ));
            }
        }
        let start = rows.len();
        rows.extend(layout::card(inner_width, &card, focused));
        if index == editor.selected_option() {
            focus = start..rows.len();
            anchor = start + border_rows;
        }
    }
    if decision.allow_comment && !decision.options.is_empty() {
        rows.extend(composition::comment_rows(editor, inner_width));
        if editor.input_comment() {
            anchor = rows.len().saturating_sub(1 + border_rows);
            focus = anchor..rows.len();
        }
    }
    if decision.allow_freeform {
        rows.extend(composition::freeform_rows(editor, inner_width));
        if editor.follows_freeform() {
            anchor = rows.len().saturating_sub(1 + border_rows);
            focus = anchor..rows.len();
        }
    }
    if let Some(error) = editor.error() {
        rows.extend(
            wrapped_content_lines(error.message.as_str(), "", inner_width)
                .into_iter()
                .map(|line| Role::Danger.style().paint(&line)),
        );
        focus = rows.len().saturating_sub(1)..rows.len();
        anchor = focus.start;
    }

    let (start, end) = editor.scroll_offset().map_or_else(
        || layout::focus_window(rows.len(), focus, anchor, capacity),
        |offset| layout::manual_window(rows.len(), offset, capacity),
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
    if decision.options.is_empty() {
        let action = if decision.require_confirmation {
            "review"
        } else {
            "submit"
        };
        return vec![modal::footer(&format!(
            "Enter: {action}  Esc: back  PgUp/PgDn: scroll"
        ))];
    }
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
    let mut focus = rows.len()..rows.len();
    let mut anchor = rows.len();
    for (index, decision) in decisions.iter().enumerate() {
        let start = rows.len();
        let marker = format!("{} ", modal::selection_marker(index == overlay.selected()));
        let card = layout::heading_rows(
            &format!(
                "{}: {}",
                owner_label(decision, session_names),
                decision.title
            ),
            &marker,
            layout::content_width(inner_width),
        );
        rows.extend(layout::card(
            inner_width,
            &card,
            index == overlay.selected(),
        ));
        if index == overlay.selected() {
            focus = start..rows.len();
            anchor = start + layout::border_rows(inner_width);
        }
    }
    let (start, end) = layout::focus_window(rows.len(), focus, anchor, capacity);
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
    fn options_and_input_fields_have_distinct_outlines_and_focus() {
        for mode in [
            UserDecisionSelectionMode::Single,
            UserDecisionSelectionMode::Multiple,
        ] {
            let workspace = WorkspaceId::new();
            let mut request = decision(workspace, None);
            request.expires_at = None;
            request.allow_comment = true;
            request.selection_mode = mode;
            request.options[0].pros = vec!["Benefit".into()];
            request.options[0].cons = vec!["Tradeoff".into()];
            let mut second = request.options[0].clone();
            second.id = "second".into();
            second.label = "Second".into();
            second.description = Some("Other description".into());
            request.options.push(second);
            let mut state = AppState::home(workspace, Vec::new());
            let _ = update(
                &mut state,
                AppEvent::Backend(BackendEvent::Decisions {
                    workspace,
                    decisions: vec![request],
                }),
            );
            let body = |state: &AppState| {
                editor_rows(state.decision_overlay().unwrap().editor().unwrap(), 70, 100)
            };
            let rows = body(&state);
            assert_eq!(rows.iter().filter(|row| row.contains('┌')).count(), 4);
            assert_eq!(rows.iter().filter(|row| row.contains('└')).count(), 4);
            let first = rows.iter().position(|row| row.contains("Safe")).unwrap();
            let second = rows.iter().position(|row| row.contains("Second")).unwrap();
            assert!(
                rows[first..second]
                    .iter()
                    .any(|row| row.contains("keep state"))
            );
            assert!(
                rows[first..second]
                    .iter()
                    .any(|row| row.contains("Pro: Benefit"))
            );
            assert!(
                rows[first..second]
                    .iter()
                    .any(|row| row.contains("Con: Tradeoff"))
            );
            assert!(rows[first..second].iter().any(|row| row.contains('└')));
            assert!(rows[first - 1].contains("\u{1b}[1;36m"));
            assert!(!rows[second - 1].contains("\u{1b}[1;36m"));
            let _ = update(&mut state, AppEvent::Key(AppKey::Tab));
            let rows = body(&state);
            let comment = rows
                .iter()
                .position(|row| row.contains("comment (optional)"))
                .unwrap();
            assert!(rows[comment - 1].contains("\u{1b}[1;36m"));
            assert!(!rows.iter().any(|row| row.contains('›')));
            let _ = update(&mut state, AppEvent::Key(AppKey::Tab));
            let rows = body(&state);
            let freeform = rows
                .iter()
                .position(|row| row.contains("> freeform:"))
                .unwrap();
            assert!(rows[freeform - 1].contains("\u{1b}[1;36m"));
        }
    }

    #[test]
    fn focused_card_stays_complete_and_oversized_descriptions_remain_scrollable() {
        let workspace = WorkspaceId::new();
        let mut request = decision(workspace, None);
        request.expires_at = None;
        request.allow_freeform = false;
        request.options[0].description = Some("Long detail\n".repeat(20) + "DETAIL_END");
        let mut second = request.options[0].clone();
        second.id = "second".into();
        second.label = "Second".into();
        second.description = Some("SECOND_DESCRIPTION".into());
        second.cons = vec!["SECOND_TRADEOFF".into()];
        request.options.push(second);
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
        assert!(body(&state).contains("Safe"));
        let mut seen = String::new();
        for _ in 0..8 {
            seen.push_str(&body(&state));
            let _ = update(&mut state, AppEvent::Key(AppKey::PageDown));
        }
        assert!(seen.contains("DETAIL_END"));
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let body = body(&state);
        assert!(body.contains("Second"));
        assert!(body.contains("SECOND_DESCRIPTION"));
        assert!(body.contains("SECOND_TRADEOFF"));
        assert!(body.contains('└'));
        assert!(body.contains("Enter: submit"));
    }

    #[test]
    fn styled_prefixes_wrap_in_terminal_cells_and_narrow_cards_keep_content() {
        let label = "日本語の長い選択肢と説明をすべて読み取る";
        for width in [0, 1, 2, 7, 8, 16, 32, 70, 120] {
            let selected = layout::heading_rows(
                label,
                &format!("{} [ ] ", modal::selection_marker(true)),
                width,
            );
            let idle = layout::heading_rows(label, "  [ ] ", width);
            assert_eq!(selected.len(), idle.len(), "focus must not change wrapping");
            for focused in [false, true] {
                let rows =
                    layout::wrapped_rows("Benefit", "      Pro: ", layout::content_width(width));
                let card = layout::card(width, &rows, focused);
                assert!(card.iter().all(|row| widgets::display_width(row) <= width));
            }
            assert!(
                selected
                    .iter()
                    .all(|row| widgets::display_width(row) <= width)
            );
        }
        let rows = layout::wrapped_rows("Benefit", "      Pro: ", 12).join("\n");
        assert!(rows.contains("Pro: Benefit"));
        assert_eq!(layout::focus_window(0, 0..0, 0, 0), (0, 0));
    }

    #[test]
    fn decision_list_editor_and_review_fit_small_and_large_terminals() {
        let workspace = WorkspaceId::new();
        let mut request = decision(workspace, None);
        request.expires_at = None;
        request.allow_comment = true;
        request.require_confirmation = true;
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Decisions {
                workspace,
                decisions: vec![request],
            }),
        );
        for keys in [
            vec![],
            vec![AppKey::Enter],
            vec![AppKey::Escape],
            vec![AppKey::Escape],
        ] {
            for key in keys {
                let _ = update(&mut state, AppEvent::Key(key));
            }
            for height in [12, 24, 50] {
                for width in [12, 20, 36, 80, 160] {
                    let frame = render_over(
                        height,
                        width,
                        &[],
                        state.decision_overlay().unwrap(),
                        state.decisions(),
                        &BTreeMap::new(),
                    );
                    assert_eq!(frame.len(), height);
                    assert!(frame.iter().all(|row| widgets::display_width(row) == width));
                    if width >= 80 {
                        assert!(frame.join("\n").contains("Esc:"));
                    }
                }
            }
        }
    }

    #[test]
    fn one_row_viewports_show_choice_labels_and_input_tails() {
        for mode in [
            UserDecisionSelectionMode::Single,
            UserDecisionSelectionMode::Multiple,
        ] {
            let workspace = WorkspaceId::new();
            let mut request = decision(workspace, None);
            request.expires_at = None;
            request.allow_comment = true;
            request.selection_mode = mode;
            let mut second = request.options[0].clone();
            second.id = "second".into();
            second.label = "Second".into();
            request.options.push(second);
            let mut state = AppState::home(workspace, Vec::new());
            let _ = update(
                &mut state,
                AppEvent::Backend(BackendEvent::Decisions {
                    workspace,
                    decisions: vec![request],
                }),
            );
            let frame = |state: &AppState| {
                render_over(
                    11,
                    80,
                    &[],
                    state.decision_overlay().unwrap(),
                    state.decisions(),
                    &BTreeMap::new(),
                )
                .join("\n")
            };
            assert!(frame(&state).contains("Safe"));
            let _ = update(&mut state, AppEvent::Key(AppKey::Down));
            assert!(frame(&state).contains("Second"));
            let _ = update(&mut state, AppEvent::Key(AppKey::Tab));
            let _ = update(
                &mut state,
                AppEvent::Key(AppKey::Paste("comment ".repeat(30) + "TAIL_COMMENT")),
            );
            assert!(frame(&state).contains("TAIL_COMMENT"));
            let _ = update(&mut state, AppEvent::Key(AppKey::Tab));
            let _ = update(
                &mut state,
                AppEvent::Key(AppKey::Paste("draft ".repeat(30) + "TAIL_FREEFORM")),
            );
            assert!(frame(&state).contains("TAIL_FREEFORM"));
            let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
            assert!(frame(&state).contains("workspace root: Choose"));
        }
    }

    #[test]
    fn paging_keeps_all_editor_and_review_card_rows_reachable_in_short_terminals() {
        for (multiple, freeform, long) in [
            (false, false, false),
            (true, false, true),
            (false, true, false),
            (false, true, true),
        ] {
            let workspace = WorkspaceId::new();
            let mut request = decision(workspace, None);
            request.expires_at = None;
            request.allow_comment = multiple;
            request.allow_freeform = freeform;
            request.require_confirmation = true;
            if freeform {
                request.options.clear();
            } else if multiple {
                request.selection_mode = UserDecisionSelectionMode::Multiple;
                request.options[0].description = Some("Detail ".repeat(30) + "DETAIL_END");
                let mut second = request.options[0].clone();
                second.id = "second".into();
                second.label = "Second".into();
                request.options.push(second);
            }
            let mut state = AppState::home(workspace, Vec::new());
            let _ = update(
                &mut state,
                AppEvent::Backend(BackendEvent::Decisions {
                    workspace,
                    decisions: vec![request],
                }),
            );
            if multiple {
                for key in [
                    AppKey::Char(' '),
                    AppKey::Down,
                    AppKey::Char(' '),
                    AppKey::Tab,
                    AppKey::Paste("Comment ".repeat(30) + "COMMENT_END"),
                ] {
                    let _ = update(&mut state, AppEvent::Key(key));
                }
            } else if freeform {
                let text = if long {
                    "日本語の回答 ".repeat(30) + "ANSWER_END"
                } else {
                    "Short answer".into()
                };
                let _ = update(&mut state, AppEvent::Key(AppKey::Paste(text)));
            }
            for review in [false, true] {
                if review {
                    assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
                    assert!(
                        state
                            .decision_overlay()
                            .unwrap()
                            .editor()
                            .unwrap()
                            .confirmation()
                            .is_some()
                    );
                }
                let full =
                    editor_rows(state.decision_overlay().unwrap().editor().unwrap(), 70, 256);
                let expected = full
                    .iter()
                    .map(|row| widgets::strip_ansi(row))
                    .filter(|row| row.contains('│'))
                    .collect::<Vec<_>>();
                assert!(!expected.is_empty());
                for height in [11, 12, 16, 24] {
                    for key in [AppKey::PageDown, AppKey::PageUp] {
                        let mut seen = String::new();
                        for page in 0..=full.len() {
                            let frame = render_over(
                                height,
                                80,
                                &[],
                                state.decision_overlay().unwrap(),
                                state.decisions(),
                                &BTreeMap::new(),
                            );
                            seen.push_str(&widgets::strip_ansi(&frame.join("\n")));
                            if page < full.len() {
                                let _ = update(&mut state, AppEvent::Key(key.clone()));
                            }
                        }
                        for row in &expected {
                            assert!(
                                seen.contains(row.trim()),
                                "unreachable card row at height={height}, review={review}, key={key:?}: {row}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn freeform_only_questions_focus_input_and_preserve_it_through_navigation() {
        use usagi_core::domain::user_decision::UserDecisionAnswer;
        for allow_comment in [false, true] {
            for confirmation in [false, true] {
                let workspace = WorkspaceId::new();
                let mut request = decision(workspace, None);
                request.expires_at = None;
                request.options.clear();
                request.allow_comment = allow_comment;
                request.require_confirmation = confirmation;
                assert!(request.validate_request().is_ok());
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
                let initial = body(&state);
                assert!(initial.contains("> freeform:"));
                assert!(!initial.contains("comment"));
                assert!(!initial.contains("choices"));
                assert!(!initial.contains("↑↓"));
                assert!(initial.contains("\u{1b}[1;36m┌"));
                for key in [
                    AppKey::Char('A'),
                    AppKey::Up,
                    AppKey::Down,
                    AppKey::Tab,
                    AppKey::DecisionPrevious,
                    AppKey::DecisionNext,
                    AppKey::Paste("lternative".into()),
                ] {
                    assert!(update(&mut state, AppEvent::Key(key)).is_empty());
                    assert!(
                        state
                            .decision_overlay()
                            .unwrap()
                            .editor()
                            .unwrap()
                            .input_freeform()
                    );
                }
                assert!(body(&state).contains("> freeform: Alternative"));
                if confirmation {
                    assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
                    assert!(body(&state).contains("Answer: Alternative"));
                    assert!(update(&mut state, AppEvent::Key(AppKey::Escape)).is_empty());
                    assert!(body(&state).contains("> freeform: Alternative"));
                    assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
                }
                assert!(
                    matches!(update(&mut state, AppEvent::Key(AppKey::Enter)).as_slice(),
                    [crate::usecase::application::controller::Effect::ResolveDecision {
                        answer: UserDecisionAnswer::Freeform {text}, ..
                    }] if text == "Alternative")
                );
            }
        }
    }

    #[test]
    fn choice_modes_preserve_answers_with_limits_comments_and_confirmation() {
        use usagi_core::domain::user_decision::{
            UserDecisionAnswer, UserDecisionRecommendation, UserDecisionSelectionLimits,
        };
        for bounds in [None, Some((1, 1)), Some((1, 2)), Some((2, 2))] {
            for allow_comment in [false, true] {
                for confirmation in [false, true] {
                    for freeform in [false, true] {
                        let workspace = WorkspaceId::new();
                        let mut request = decision(workspace, None);
                        request.expires_at = None;
                        request.allow_comment = allow_comment;
                        request.allow_freeform = freeform;
                        request.require_confirmation = confirmation;
                        let mut second = request.options[0].clone();
                        second.id = "second".into();
                        second.label = "Second".into();
                        request.options.push(second);
                        if let Some((min, max)) = bounds {
                            request.selection_mode = UserDecisionSelectionMode::Multiple;
                            request.selection_limits =
                                Some(UserDecisionSelectionLimits { min, max });
                        }
                        let ids = if bounds == Some((2, 2)) {
                            vec!["safe".to_owned(), "second".to_owned()]
                        } else {
                            vec!["safe".to_owned()]
                        };
                        request.recommendation = Some(UserDecisionRecommendation {
                            option_ids: ids.clone(),
                            reason: "Recommendation reason".into(),
                        });
                        assert!(request.validate_request().is_ok());
                        let mut state = AppState::home(workspace, Vec::new());
                        let _ = update(
                            &mut state,
                            AppEvent::Backend(BackendEvent::Decisions {
                                workspace,
                                decisions: vec![request],
                            }),
                        );
                        let body = |state: &AppState| {
                            editor_rows(
                                state.decision_overlay().unwrap().editor().unwrap(),
                                70,
                                100,
                            )
                            .join("\n")
                        };
                        assert!(body(&state).contains("Safe [recommended]"));
                        assert_eq!(body(&state).contains("freeform:"), freeform);
                        assert_eq!(body(&state).contains("comment (optional)"), allow_comment);
                        if bounds.is_some() {
                            assert!(!body(&state).contains("[x]"));
                            let _ = update(&mut state, AppEvent::Key(AppKey::Char(' ')));
                            if ids.len() == 2 {
                                let _ = update(&mut state, AppEvent::Key(AppKey::Down));
                                let _ = update(&mut state, AppEvent::Key(AppKey::Char(' ')));
                            }
                            assert!(body(&state).contains("[x]"));
                        }
                        if allow_comment {
                            let _ = update(&mut state, AppEvent::Key(AppKey::Tab));
                            let _ = update(
                                &mut state,
                                AppEvent::Key(AppKey::Paste("Only staging".into())),
                            );
                        }
                        if confirmation {
                            assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
                            assert!(body(&state).contains("Choice: Safe [safe]"));
                        }
                        let comment = allow_comment.then(|| "Only staging".to_owned());
                        let expected = if bounds.is_some() {
                            UserDecisionAnswer::Options {
                                option_ids: ids,
                                comment,
                            }
                        } else {
                            UserDecisionAnswer::Option {
                                option_id: "safe".into(),
                                comment,
                            }
                        };
                        assert!(
                            matches!(update(&mut state, AppEvent::Key(AppKey::Enter)).as_slice(),
                            [crate::usecase::application::controller::Effect::ResolveDecision {answer, ..}]
                                if answer == &expected)
                        );
                    }
                }
            }
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

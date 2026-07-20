//! Durable user-decision list and answer editor overlays.

#![coverage(off)] // ANSI-only composition over the shared modal primitive; state transitions are covered in the controller.

use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::{self, modal};
use crate::usecase::application::controller::DecisionOverlayState;

const INNER_WIDTH: usize = 62;

/// Render either the workspace pending list or the selected decision editor.
#[must_use]
#[coverage(off)] // Pure ANSI composition follows the shared modal primitive; controller/render integration is covered by workspace views.
pub fn render_over(
    height: usize,
    width: usize,
    base: &[String],
    overlay: &DecisionOverlayState,
    decisions: &[usagi_core::domain::user_decision::UserDecision],
) -> Vec<String> {
    let (title, body) = if let Some(editor) = overlay.editor() {
        let decision = editor.decision();
        let mut body = vec![modal::heading(&decision.title)];
        body.extend(
            decision
                .prompt
                .lines()
                .map(|line| modal::content_line(line, INNER_WIDTH)),
        );
        if let Some(deadline) = decision.expires_at {
            body.push(modal::caption(&format!(
                "expires: {}",
                deadline.format("%Y-%m-%d %H:%M UTC")
            )));
        }
        body.push(String::new());
        for (index, option) in decision.options.iter().enumerate() {
            // The decision picker shares the list shape's cursor with the other
            // selection lists (#374): the danger `›` marker via `content_line`.
            body.push(modal::content_line(
                &format!(
                    "{} {}",
                    modal::selection_marker(index == editor.selected_option()),
                    option.label
                ),
                INNER_WIDTH,
            ));
            if let Some(description) = &option.description {
                body.push(Style::new().dim().paint(&widgets::clip_to_width(
                    &format!("     {description}"),
                    INNER_WIDTH,
                )));
            }
        }
        if decision.allow_freeform {
            body.push(String::new());
            body.push(modal::content_line(
                &format!("freeform: {}", editor.freeform()),
                INNER_WIDTH,
            ));
        }
        if let Some(error) = editor.error() {
            body.push(
                Role::Danger
                    .style()
                    .paint(&format!("  {}", error.message.as_str())),
            );
        }
        body.push(modal::footer("↑↓: select   Enter: submit   Esc: back"));
        ("User decision", body)
    } else {
        let mut body = vec![modal::caption("Pending decisions for this workspace")];
        if decisions.is_empty() {
            body.push(modal::empty_notice("(none)"));
        }
        for (index, decision) in decisions.iter().enumerate() {
            body.push(modal::content_line(
                &format!(
                    "{} {}",
                    modal::selection_marker(index == overlay.selected()),
                    decision.title
                ),
                INNER_WIDTH,
            ));
        }
        body.push(String::new());
        body.push(modal::footer("↑↓: select   Enter: open   Esc: close"));
        ("Pending decisions", body)
    };
    modal::render_over(
        height,
        width,
        base,
        title,
        INNER_WIDTH,
        &modal::fixed_body(body, 16),
    )
}

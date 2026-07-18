//! Durable user-decision list and answer editor overlays.

use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::{self, modal};
use crate::usecase::application::controller::DecisionOverlayState;

const INNER_WIDTH: usize = 62;

/// Render either the workspace pending list or the selected decision editor.
#[must_use]
pub fn render_over(
    height: usize,
    width: usize,
    base: &[String],
    overlay: &DecisionOverlayState,
    decisions: &[usagi_core::domain::user_decision::UserDecision],
) -> Vec<String> {
    let (title, body) = if let Some(editor) = overlay.editor() {
        let decision = editor.decision();
        let mut body = vec![
            Role::Accent
                .style()
                .bold()
                .paint(&format!("  {}", decision.title)),
        ];
        body.extend(
            decision
                .prompt
                .lines()
                .map(|line| widgets::clip_to_width(&format!("  {line}"), INNER_WIDTH)),
        );
        if let Some(deadline) = decision.expires_at {
            body.push(Style::new().dim().paint(&format!(
                "  expires: {}",
                deadline.format("%Y-%m-%d %H:%M UTC")
            )));
        }
        body.push(String::new());
        for (index, option) in decision.options.iter().enumerate() {
            let marker = if index == editor.selected_option() {
                ">"
            } else {
                " "
            };
            body.push(widgets::clip_to_width(
                &format!(" {marker} {}", option.label),
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
            body.push(widgets::clip_to_width(
                &format!("  freeform: {}", editor.freeform()),
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
        body.push(
            Style::new()
                .dim()
                .paint("  ↑↓: select   Enter: submit   Esc: back"),
        );
        ("User decision", body)
    } else {
        let mut body = vec![
            Style::new()
                .dim()
                .paint("  Pending decisions for this workspace"),
        ];
        if decisions.is_empty() {
            body.push(Style::new().dim().paint("  (none)"));
        }
        for (index, decision) in decisions.iter().enumerate() {
            let marker = if index == overlay.selected() {
                ">"
            } else {
                " "
            };
            body.push(widgets::clip_to_width(
                &format!(" {marker} {}", decision.title),
                INNER_WIDTH,
            ));
        }
        body.push(String::new());
        body.push(
            Style::new()
                .dim()
                .paint("  ↑↓: select   Enter: open   Esc: close"),
        );
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

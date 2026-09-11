//! Fixed progress header and instruction composer around a scrollable history.

use crate::presentation::widgets::{clip_to_width, pad_to_width};
use crate::usecase::application::workflow::WorkflowPanel;
use usagi_core::domain::workflow::Delivery;

/// Render within the pane's content rectangle, never over its tab strip.
#[must_use]
pub fn render(height: usize, width: usize, panel: &WorkflowPanel) -> Vec<String> {
    if height == 0 || width == 0 {
        return vec![String::new(); height];
    }
    let header = header(panel);
    let mut composer = vec![if panel.run.is_some() {
        format!("Instruction to: {}", panel.recipient_label())
    } else {
        "Goal".into()
    }];
    composer.extend(input_rows(panel, width));
    composer.push(if panel.submitting {
        "Submitting... (draft retained)".into()
    } else if panel.pending.is_some() {
        "Ctrl+S: retry previous request (same operation ID)".into()
    } else if panel.run.is_some() {
        "Enter: newline | Tab: recipient | Ctrl+S: submit".into()
    } else {
        "Tab: goal/agents | Left/Right: choose | Ctrl+S: start".into()
    });

    // Even a short terminal keeps one status row and the end of the composer.
    let composer_height = composer.len().min(height.saturating_sub(1));
    let header_height = header.len().min(height - composer_height);
    let history_height = height - composer_height - header_height;
    let mut rows = header
        .into_iter()
        .take(header_height)
        .map(|line| safe_line(&line))
        .collect::<Vec<_>>();
    let history = panel.run.as_ref().map_or_else(Vec::new, |run| {
        let mut rows = run
            .history
            .iter()
            .map(|entry| format!("{}: {}", entry.actor, entry.body.replace('\n', " / ")))
            .collect::<Vec<_>>();
        rows.extend(
            run.instructions
                .iter()
                .map(|instruction| {
                    let delivery = match instruction.delivery {
                        Delivery::Queued => "queued",
                        Delivery::Notified => "notified",
                        Delivery::Acknowledged => "processed",
                        Delivery::Unconfirmed => "delivery unconfirmed",
                    };
                    format!("[{delivery}] {}", instruction.body.replace('\n', " / "))
                })
                .collect::<Vec<_>>(),
        );
        rows
    });
    let end = history.len().saturating_sub(panel.history_offset);
    let start = end.saturating_sub(history_height);
    rows.extend(history[start..end].iter().map(|line| safe_line(line)));
    rows.resize(header_height + history_height, String::new());
    rows.extend(
        composer
            .into_iter()
            .rev()
            .take(composer_height)
            .collect::<Vec<_>>()
            .into_iter()
            .rev(),
    );
    rows.into_iter()
        .map(|line| pad_to_width(&clip_to_width(&line, width), width))
        .collect()
}

fn header(panel: &WorkflowPanel) -> Vec<String> {
    let status = if panel.loading && panel.run.is_none() {
        "Loading workflow".to_owned()
    } else {
        panel.run.as_ref().map_or_else(
            || "Implementation + Review / Not started".to_owned(),
            |run| format!("Implementation + Review / {}", run.phase.label()),
        )
    };
    let mut header = vec![status];
    if let Some(run) = &panel.run {
        let owner = match run.phase {
            usagi_core::domain::workflow::Phase::Reviewing => run.agents.reviewer.selector(),
            usagi_core::domain::workflow::Phase::Waiting => "Human decision",
            usagi_core::domain::workflow::Phase::Ready => "None (complete)",
            _ => run.agents.implementer.selector(),
        };
        header.push(format!("Current owner: {owner}"));
        header.push(format!(
            "Implement -> Review -> PR ready | revisions {}/{}",
            run.revisions, run.revision_limit
        ));
        if let Some(reason) = &run.waiting_reason {
            header.push(reason.clone());
        }
        if let Some(review) = &run.review {
            header.push(format!("Review HEAD: {}", review.target.head_sha));
        }
    } else {
        for (index, label, provider) in [
            (0, "Planner", panel.agents.planner),
            (1, "Implementer", panel.agents.implementer),
            (2, "Reviewer", panel.agents.reviewer),
        ] {
            let focus = if panel.agent_field == Some(index) {
                ">"
            } else {
                " "
            };
            let name = if provider == usagi_core::domain::settings::DefaultModel::Agy {
                "Gemini (agy)"
            } else {
                provider.selector()
            };
            header.push(format!("{focus} {label}: < {name} >"));
        }
    }
    if let Some(error) = &panel.error {
        header.push(format!("Error: {error}"));
    }
    header
}

fn safe_line(line: &str) -> String {
    let line = line.replace(
        [
            crate::presentation::frame::INPUT_CURSOR_MARKER,
            crate::presentation::frame::TERMINAL_CURSOR_MARKER,
        ],
        "",
    );
    usagi_core::domain::presentation_text::sanitize_presentation_line(&line)
}

fn input_rows(panel: &WorkflowPanel, width: usize) -> Vec<String> {
    let value = panel.draft.value();
    let cursor = panel.draft.cursor();
    let current = value[..cursor]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count();
    let line_start = value[..cursor].rfind('\n').map_or(0, |index| index + 1);
    value
        .split('\n')
        .enumerate()
        .skip(current.saturating_sub(2))
        .take(3)
        .map(|(index, line)| {
            let safe = safe_line(line);
            if index != current {
                return format!("> {safe}");
            }
            let caret = safe_line(&line[..cursor - line_start]).len();
            let mut start = 0;
            while start < caret
                && crate::presentation::widgets::display_width(&safe[start..caret])
                    >= width.saturating_sub(3)
            {
                start += safe[start..].chars().next().map_or(0, char::len_utf8);
            }
            format!(
                "> {}",
                crate::presentation::widgets::block_caret(
                    &safe[start..],
                    caret - start,
                    &crate::presentation::theme::Style::new()
                )
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_renders_selected_providers_and_focus() {
        use usagi_core::domain::settings::DefaultModel;
        let mut panel = WorkflowPanel::default();
        panel.agents.planner = DefaultModel::Agy;
        panel.agents.implementer = DefaultModel::Claude;
        panel.agents.reviewer = DefaultModel::OpenAi;
        panel.agent_field = Some(1);
        let rendered = render(20, 100, &panel).join("\n");
        assert!(rendered.contains("Planner: < Gemini (agy) >"));
        assert!(rendered.contains("> Implementer: < claude >"));
        assert!(rendered.contains("Reviewer: < codex >"));
        let mut run = crate::usecase::application::workflow::fixture_run(
            usagi_core::domain::id::SessionId::new(),
        );
        run.agents = panel.agents;
        panel.run = Some(run);
        assert!(
            render(20, 100, &panel)
                .join("\n")
                .contains("Current owner: claude")
        );
        panel.run.as_mut().unwrap().phase = usagi_core::domain::workflow::Phase::Reviewing;
        assert!(
            render(20, 100, &panel)
                .join("\n")
                .contains("Current owner: codex")
        );
    }

    #[test]
    fn renders_real_progress_history_delivery_and_unconfirmed_controls_safely() {
        use usagi_core::domain::id::{OperationId, SessionId};
        use usagi_core::domain::workflow::{
            Instruction, Phase, Recipient, Review, WorkflowCommand, WorkflowHistoryEntry,
        };
        let mut run = crate::usecase::application::workflow::fixture_run(SessionId::new());
        run.waiting_reason = Some("Check results pending".into());
        run.review = Some(Review {
            request: OperationId::new(),
            target: usagi_core::domain::agent_message::ReviewTarget {
                base_sha: "a".repeat(40),
                head_sha: "b".repeat(40),
            },
            approved: true,
        });
        run.history.push(WorkflowHistoryEntry {
            id: OperationId::new(),
            actor: "Claude".into(),
            body: "Review completed".into(),
        });
        for delivery in [
            Delivery::Queued,
            Delivery::Notified,
            Delivery::Acknowledged,
            Delivery::Unconfirmed,
        ] {
            run.instructions.push(Instruction {
                id: OperationId::new(),
                requested_recipient: Recipient::Automatic,
                recipient: run.implementer,
                body: "Check\nerrors".into(),
                delivery,
            });
        }
        let mut panel = WorkflowPanel {
            run: Some(run),
            error: Some("\u{1b}[2Junsafe\u{202e}".into()),
            ..WorkflowPanel::default()
        };
        panel
            .draft
            .replace("line one\nline two\nline three\nline four");
        for phase in [
            Phase::Starting,
            Phase::Implementing,
            Phase::Reviewing,
            Phase::Revising,
            Phase::Verifying,
            Phase::Waiting,
            Phase::Ready,
        ] {
            panel.run.as_mut().unwrap().phase = phase;
            let view = render(25, 90, &panel).join("\n");
            assert!(view.contains(phase.label()));
            assert!(view.contains("Review completed"));
            assert!(view.contains("delivery unconfirmed"));
            assert!(!view.contains("\u{1b}[2J"));
            assert!(!view.contains('\u{202e}'));
        }
        panel.submitting = true;
        assert!(render(20, 90, &panel).join("\n").contains("Submitting"));
        panel.submitting = false;
        panel.pending = Some((
            OperationId::new(),
            WorkflowCommand::Start {
                goal: "task".into(),
                agents: usagi_core::domain::workflow::WorkflowAgents::default(),
            },
        ));
        assert!(render(20, 90, &panel).join("\n").contains("retry previous"));
        panel.run = None;
        panel.loading = true;
        assert!(
            render(20, 90, &panel)
                .join("\n")
                .contains("Loading workflow")
        );
        panel.draft.move_edge(false);
        assert_eq!(input_rows(&panel, 10).len(), 3);
        panel.draft.replace("日本語の非常に長い入力を確認する");
        assert!(
            input_rows(&panel, 12)[0].contains(crate::presentation::frame::INPUT_CURSOR_MARKER)
        );
    }

    #[test]
    fn narrow_and_short_views_keep_bounds_and_the_composer() {
        let mut panel = WorkflowPanel::default();
        panel
            .draft
            .replace("Check authentication\nInclude error cases");
        for height in 0..16 {
            for width in [0, 1, 12, 80] {
                let rows = render(height, width, &panel);
                assert_eq!(rows.len(), height);
                assert!(
                    rows.iter()
                        .all(|row| crate::presentation::widgets::display_width(row) <= width)
                );
            }
        }
        let rows = render(16, 80, &panel).join("\n");
        assert!(rows.contains("Not started"));
        assert!(rows.contains("Check authentication"));
        assert!(rows.contains("Ctrl+S"));
    }
}

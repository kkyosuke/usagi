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
    let status = if panel.loading {
        "Loading workflow".to_owned()
    } else {
        panel.run.as_ref().map_or_else(
            || "Implementation + Review / Not started".to_owned(),
            |run| format!("Implementation + Review / {}", run.phase.label()),
        )
    };
    let mut header = vec![status];
    if let Some(run) = &panel.run {
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
        header.push("Codex: implementation | Claude: review | revision limit: 3".into());
    }
    if let Some(error) = &panel.error {
        header.push(format!("Error: {error}"));
    }
    let mut composer = vec![if panel.run.is_some() {
        format!("Instruction to: {}", panel.recipient_label())
    } else {
        "Goal".into()
    }];
    let draft = panel.draft.value();
    let mut input = draft.lines().rev().take(3).collect::<Vec<_>>();
    input.reverse();
    if input.is_empty() {
        composer.push("> ".into());
    } else {
        composer.extend(input.into_iter().map(|line| format!("> {line}")));
    }
    composer.push(if panel.submitting {
        "Submitting... (draft retained)".into()
    } else {
        "Enter: newline | Tab: recipient | Ctrl+S: submit".into()
    });

    // Even a short terminal keeps one status row and the end of the composer.
    let composer_height = composer.len().min(height.saturating_sub(1));
    let header_height = header.len().min(height - composer_height);
    let history_height = height - composer_height - header_height;
    let mut rows = header.into_iter().take(header_height).collect::<Vec<_>>();
    let history = panel.run.as_ref().map_or_else(Vec::new, |run| {
        run.instructions
            .iter()
            .map(|instruction| {
                let delivery = match instruction.delivery {
                    Delivery::Queued => "queued",
                    Delivery::Notified => "notified",
                    Delivery::Acknowledged => "processed",
                };
                format!("[{delivery}] {}", instruction.body.replace('\n', " / "))
            })
            .collect::<Vec<_>>()
    });
    let end = history.len().saturating_sub(panel.history_offset);
    let start = end.saturating_sub(history_height);
    rows.extend(history[start..end].iter().cloned());
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
        .map(|line| {
            let safe = usagi_core::domain::presentation_text::sanitize_presentation_line(&line);
            pad_to_width(&clip_to_width(&safe, width), width)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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

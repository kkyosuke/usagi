//! Responsive decision tables and horizontally scrollable preformatted diagrams.

use unicode_segmentation::UnicodeSegmentation;
use usagi_core::domain::presentation_text::sanitize_presentation_line;
use usagi_core::domain::user_decision::UserDecisionContext;

use super::{modal, widgets, wrapped_content_lines};

pub(super) fn render(block: &UserDecisionContext, width: usize, column: usize) -> Vec<String> {
    match block {
        UserDecisionContext::Table {
            title,
            columns,
            rows,
        } => {
            let mut output = wrapped_content_lines(title, "", width);
            let available = width.saturating_sub(modal::BODY_INDENT_WIDTH);
            // Preserve every cell at narrow widths, using labelled records when
            // a grid would leave less than six terminal cells per column.
            let cell_width = available.saturating_sub(columns.len().saturating_sub(1) * 3)
                / columns.len().max(1);
            if cell_width < 6 {
                for row in rows {
                    output.push(String::new());
                    for (label, cell) in columns.iter().zip(row) {
                        output.extend(wrapped_content_lines(
                            &format!("{label}: {cell}"),
                            "",
                            width,
                        ));
                    }
                }
            } else {
                output.extend(table_row(columns, cell_width, width));
                output.push(modal::content_line(
                    &vec!["─".repeat(cell_width); columns.len()].join("─┼─"),
                    width,
                ));
                for row in rows {
                    output.extend(table_row(row, cell_width, width));
                }
            }
            output
        }
        UserDecisionContext::Diagram { title, text } => {
            let mut output = wrapped_content_lines(title, "", width);
            output.extend(text.lines().map(|line| {
                let safe = sanitize_presentation_line(line);
                let mut skipped = 0;
                let visible: String = safe
                    .graphemes(true)
                    .skip_while(|ch| {
                        if skipped >= column {
                            return false;
                        }
                        skipped += widgets::display_width(ch);
                        true
                    })
                    .collect();
                // A wide grapheme crossing the left viewport edge still
                // occupies its remaining cells, keeping diagram edges aligned.
                let visible = format!("{}{}", " ".repeat(skipped.saturating_sub(column)), visible);
                modal::content_line(
                    &widgets::clip_to_width(
                        &visible,
                        width.saturating_sub(modal::BODY_INDENT_WIDTH),
                    ),
                    width,
                )
            }));
            output.extend(wrapped_content_lines("←/→: pan diagram", "", width));
            output
        }
    }
}

fn table_row(cells: &[String], cell_width: usize, width: usize) -> Vec<String> {
    let wrapped: Vec<Vec<String>> = cells
        .iter()
        .map(|cell| {
            cell.split('\n')
                .flat_map(|line| {
                    let mut lines =
                        widgets::wrap_to_width(&sanitize_presentation_line(line), cell_width);
                    if lines.is_empty() {
                        lines.push(String::new());
                    }
                    lines
                })
                .collect()
        })
        .collect();
    (0..wrapped.iter().map(Vec::len).max().unwrap_or_default())
        .map(|index| {
            let row = wrapped
                .iter()
                .map(|cell| {
                    widgets::pad_to_width(cell.get(index).map_or("", String::as_str), cell_width)
                })
                .collect::<Vec<_>>()
                .join(" │ ");
            modal::content_line(&row, width)
        })
        .collect()
}

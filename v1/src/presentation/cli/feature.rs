//! The `usagi feature` command: print the agent-feature support matrix.
//!
//! Renders the [`AgentFeature`] × [`AgentCli`] support table from the domain's
//! single source of truth ([`crate::domain::agent_feature`]), so users can see at
//! a glance which usagi integrations each agent CLI receives.

use crate::domain::agent_feature::{support, AgentFeature, Support};
use crate::domain::settings::AgentCli;
use unicode_width::UnicodeWidthStr;

/// The CLIs shown as columns, left to right: every [`AgentCli`] in canonical
/// order, labelled by its display name (the domain's single source of truth).
const COLUMNS: [AgentCli; 5] = AgentCli::ALL;

/// Entry point for `usagi feature`: print the support matrix.
pub fn run() -> anyhow::Result<()> {
    for line in render() {
        println!("{line}");
    }
    Ok(())
}

/// The lines printed by `usagi feature`: a short title, a terminal-friendly
/// support matrix, and a legend. The matrix is drawn as a box table instead of a
/// Markdown table so the command reads well directly in a terminal. Widths are
/// measured with [`UnicodeWidthStr`] because the rows mix Japanese labels and
/// status symbols.
fn render() -> Vec<String> {
    let headers: Vec<String> = std::iter::once("機能".to_string())
        .chain(COLUMNS.iter().map(|cli| cli.display_name().to_string()))
        .collect();
    let rows: Vec<Vec<String>> = AgentFeature::ALL
        .into_iter()
        .map(|feature| {
            let mut cells = vec![feature.label().to_string()];
            cells.extend(
                COLUMNS
                    .iter()
                    .map(|cli| feature_cell(support(*cli, feature))),
            );
            cells
        })
        .collect();

    let alignments = std::iter::once(Alignment::Left)
        .chain(COLUMNS.iter().map(|_| Alignment::Center))
        .collect::<Vec<_>>();
    let table = render_table(&headers, &rows, &alignments);

    let mut lines = vec![
        "usagi feature".to_string(),
        "各 Agent CLI に組み込む機能の対応状況".to_string(),
        String::new(),
    ];
    lines.extend(table);
    lines.push(String::new());
    lines.push("凡例: yes = usagi が配線 / no = CLI 制約により非対応".to_string());
    lines.push(
        "注: Gemini・Antigravity は MCP・フック・system prompt のインライン注入経路が無いため非対応\
         （状態はターミナルベルで推定する）。"
            .to_string(),
    );
    lines
}

fn feature_cell(support: Support) -> String {
    let label = match support {
        Support::Yes => "yes",
        Support::No => "no",
    };
    format!("{} {label}", support.glyph())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Alignment {
    Left,
    Center,
}

fn render_table(headers: &[String], rows: &[Vec<String>], alignments: &[Alignment]) -> Vec<String> {
    debug_assert_eq!(headers.len(), alignments.len());
    debug_assert!(rows.iter().all(|row| row.len() == headers.len()));

    let widths = column_widths(headers, rows);
    let mut lines = Vec::with_capacity(rows.len() + 4);

    lines.push(border('┌', '┬', '┐', &widths));
    lines.push(table_row(headers, &widths, alignments));
    lines.push(border('├', '┼', '┤', &widths));
    for row in rows {
        lines.push(table_row(row, &widths, alignments));
    }
    lines.push(border('└', '┴', '┘', &widths));

    lines
}

fn column_widths(headers: &[String], rows: &[Vec<String>]) -> Vec<usize> {
    let mut widths: Vec<usize> = headers.iter().map(|cell| display_width(cell)).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(display_width(cell));
        }
    }
    widths
}

fn border(left: char, join: char, right: char, widths: &[usize]) -> String {
    let segments = widths
        .iter()
        .map(|width| "─".repeat(width + 2))
        .collect::<Vec<_>>()
        .join(&join.to_string());
    format!("{left}{segments}{right}")
}

fn table_row(cells: &[String], widths: &[usize], alignments: &[Alignment]) -> String {
    let rendered_cells = cells
        .iter()
        .zip(widths)
        .zip(alignments)
        .map(|((cell, width), alignment)| padded_cell(cell, *width, *alignment))
        .collect::<Vec<_>>()
        .join("│");
    format!("│{rendered_cells}│")
}

fn padded_cell(cell: &str, width: usize, alignment: Alignment) -> String {
    let padding = width.saturating_sub(display_width(cell));
    match alignment {
        Alignment::Left => format!(" {cell}{} ", " ".repeat(padding)),
        Alignment::Center => {
            let left = padding / 2;
            let right = padding - left;
            format!(" {}{cell}{} ", " ".repeat(left), " ".repeat(right))
        }
    }
}

fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_lists_every_feature_row() {
        let lines = render();
        // Each feature label appears as a row.
        for feature in AgentFeature::ALL {
            assert!(
                lines.iter().any(|line| line.contains(feature.label())),
                "missing row for {:?}",
                feature
            );
        }
    }

    #[test]
    fn render_has_a_header_with_each_cli_column() {
        let lines = render();
        let header = lines.iter().find(|line| line.contains("│ 機能")).unwrap();
        for cli in COLUMNS {
            let label = cli.display_name();
            assert!(header.contains(label), "header missing column {label}");
        }
        assert!(lines.iter().any(|line| line.starts_with('┌')));
        assert!(lines.iter().any(|line| line.starts_with('└')));
    }

    #[test]
    fn render_shows_the_matrix_support_per_cli() {
        let lines = render();
        // Claude and Gemini both support MCP now, so the MCP row has no no-cell.
        let mcp_row = lines
            .iter()
            .find(|line| line.contains(AgentFeature::Mcp.label()))
            .unwrap();
        assert!(mcp_row.contains(&feature_cell(Support::Yes)));
        assert!(!mcp_row.contains(&feature_cell(Support::No)));

        // Phase reporting is yes for Claude, but no for Gemini.
        let phase_row = lines
            .iter()
            .find(|line| line.contains(AgentFeature::PhaseReporting.label()))
            .unwrap();
        assert!(phase_row.contains(&feature_cell(Support::Yes)));
        assert!(phase_row.contains(&feature_cell(Support::No)));

        // Gemini's resume row is supported, so that row has no no-cell — every
        // column is yes.
        let resume_row = lines
            .iter()
            .find(|line| line.contains(AgentFeature::Resume.label()))
            .unwrap();
        assert!(!resume_row.contains(&feature_cell(Support::No)));
    }

    #[test]
    fn render_table_uses_equal_display_width_for_every_line() {
        let lines = render();
        let table_lines: Vec<_> = lines
            .iter()
            .filter(|line| {
                line.starts_with('┌')
                    || line.starts_with('│')
                    || line.starts_with('├')
                    || line.starts_with('└')
            })
            .collect();
        let width = display_width(table_lines[0]);
        let all_lines_are_aligned = table_lines.iter().all(|line| display_width(line) == width);
        let diagnostics = table_lines
            .iter()
            .map(|line| format!("{}: {line}", display_width(line)))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            all_lines_are_aligned,
            "table lines should align:\n{diagnostics}"
        );
    }

    #[test]
    fn render_ends_with_a_legend() {
        let lines = render();
        assert!(lines.iter().any(|line| line.contains("凡例")));
        assert!(lines.iter().any(|line| line.contains("ターミナルベル")));
    }

    #[test]
    fn run_prints_without_error() {
        // The thin print wrapper renders the same matrix and succeeds.
        assert!(run().is_ok());
    }

    #[test]
    fn display_width_counts_japanese_as_wide() {
        assert_eq!(display_width("機能"), 4);
        assert_eq!(display_width(&feature_cell(Support::Yes)), 5);
    }
}

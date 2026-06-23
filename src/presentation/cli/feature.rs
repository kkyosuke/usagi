//! The `usagi feature` command: print the agent-feature support matrix.
//!
//! Renders the [`AgentFeature`] × [`AgentCli`] support table from the domain's
//! single source of truth ([`crate::domain::agent_feature`]), so users can see at
//! a glance which usagi integrations each agent CLI receives.

use crate::domain::agent_feature::{support, AgentFeature};
use crate::domain::settings::AgentCli;

/// The CLIs shown as columns, left to right: every [`AgentCli`] in canonical
/// order, labelled by its display name (the domain's single source of truth).
const COLUMNS: [AgentCli; 4] = AgentCli::ALL;

/// Entry point for `usagi feature`: print the support matrix.
pub fn run() -> anyhow::Result<()> {
    for line in render() {
        println!("{line}");
    }
    Ok(())
}

/// The lines printed by `usagi feature`: a title, a Markdown table of the support
/// matrix (features as rows, CLIs as columns), and a legend. A Markdown table
/// keeps the columns readable without per-cell width math over mixed-width CJK
/// labels and emoji, and stays copy-pasteable.
fn render() -> Vec<String> {
    let mut lines = vec![
        "usagi が各 Agent CLI に組み込む機能の対応状況:".to_string(),
        String::new(),
    ];

    let header: Vec<&str> = std::iter::once("機能")
        .chain(COLUMNS.iter().map(|cli| cli.display_name()))
        .collect();
    lines.push(format!("| {} |", header.join(" | ")));

    // The separator centers the CLI columns and left-aligns the feature column.
    let mut separator = vec!["---".to_string()];
    separator.extend(COLUMNS.iter().map(|_| ":---:".to_string()));
    lines.push(format!("| {} |", separator.join(" | ")));

    for feature in AgentFeature::ALL {
        let mut cells = vec![feature.label().to_string()];
        cells.extend(
            COLUMNS
                .iter()
                .map(|cli| support(*cli, feature).glyph().to_string()),
        );
        lines.push(format!("| {} |", cells.join(" | ")));
    }

    lines.push(String::new());
    lines.push("凡例: ✅ usagi が配線 / ❌ CLI 制約により非対応".to_string());
    lines.push(
        "注: Gemini は MCP・フック・system prompt のインライン注入経路が無いため非対応\
         （状態はターミナルベルで推定する）。"
            .to_string(),
    );
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent_feature::Support;

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
        let header = lines
            .iter()
            .find(|line| line.starts_with("| 機能 |"))
            .unwrap();
        for cli in COLUMNS {
            let label = cli.display_name();
            assert!(header.contains(label), "header missing column {label}");
        }
        // The alignment separator row follows the header.
        assert!(lines.iter().any(|line| line.contains(":---:")));
    }

    #[test]
    fn render_shows_the_matrix_glyphs_per_cli() {
        let lines = render();
        // Claude's MCP row cell is ✅; Gemini's is ❌ — the row carries both.
        let mcp_row = lines
            .iter()
            .find(|line| line.contains(AgentFeature::Mcp.label()))
            .unwrap();
        assert!(mcp_row.contains(Support::Yes.glyph()));
        assert!(mcp_row.contains(Support::No.glyph()));

        // Gemini's resume row is supported, so that row has no ❌ from Gemini —
        // every column is ✅.
        let resume_row = lines
            .iter()
            .find(|line| line.contains(AgentFeature::Resume.label()))
            .unwrap();
        assert!(!resume_row.contains(Support::No.glyph()));
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
}

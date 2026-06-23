//! Output formatting for `usagi issue`: human-readable listings/groups and the
//! `--json` serialisations.

use anyhow::Result;

use crate::presentation::cli::render::json_lines;
use crate::usecase::issue::{
    group, list_line, stats_line, GroupBy, IssueStats, ListedIssue, ListedIssueView,
};

/// Render a listing (from `list` or `search`) either as JSON or as aligned
/// human-readable lines.
pub(super) fn render_listing(items: Vec<ListedIssue>, json: bool) -> Result<Vec<String>> {
    if json {
        let views: Vec<ListedIssueView> = items.iter().map(ListedIssueView::from).collect();
        return json_lines(&views);
    }
    Ok(render_list(&items))
}

/// Render a listing split into labelled groups, each with its own progress
/// footer, followed by an overall total.
pub(super) fn render_grouped(items: Vec<ListedIssue>, axis: GroupBy) -> Vec<String> {
    if items.is_empty() {
        return vec!["No issues found.".to_string()];
    }
    let overall = IssueStats::from_listed(&items);
    let mut out = Vec::new();
    for (label, group_items) in group(items, axis) {
        out.push(format!("== {label} =="));
        out.extend(render_list(&group_items));
        out.push(format!(
            "   {}",
            stats_line(&IssueStats::from_listed(&group_items))
        ));
        out.push(String::new());
    }
    out.push(stats_line(&overall));
    out
}

/// Format a listing as aligned, one-line-per-issue text. The per-line layout and
/// progress footer live in [`crate::usecase::issue`] so the CLI and the TUI
/// `issue` command render identically.
pub(super) fn render_list(items: &[ListedIssue]) -> Vec<String> {
    if items.is_empty() {
        return vec!["No issues found.".to_string()];
    }
    items.iter().map(list_line).collect()
}

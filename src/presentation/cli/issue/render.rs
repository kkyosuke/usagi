//! Output formatting for `usagi issue`: human-readable listings/groups and the
//! `--json` serialisations.

use anyhow::Result;
use serde::Serialize;

use crate::domain::issue::IssueStatus;
use crate::usecase::issue::{group, GroupBy, IssueStats, ListedIssue, ListedIssueView};

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
            format_stats(&IssueStats::from_listed(&group_items))
        ));
        out.push(String::new());
    }
    out.push(format_stats(&overall));
    out
}

/// A one-line progress summary: totals, completion, and readiness.
pub(super) fn format_stats(stats: &IssueStats) -> String {
    format!(
        "{} issues · {} done ({}%) · {} ready  {}",
        stats.total,
        stats.done,
        stats.completion_percent(),
        stats.ready,
        stats.progress_bar(20),
    )
}

/// Format a listing as aligned, one-line-per-issue text.
pub(super) fn render_list(items: &[ListedIssue]) -> Vec<String> {
    if items.is_empty() {
        return vec!["No issues found.".to_string()];
    }
    items
        .iter()
        .map(|l| {
            let marker = readiness(l);
            let mut line = format!(
                "#{:<3} {:<12} {:<6} {:<8} {}",
                l.summary.number,
                l.summary.status.as_str(),
                l.summary.priority.as_str(),
                marker,
                l.summary.title,
            );
            if !l.unmet_deps.is_empty() {
                line.push_str(&format!("  (blocked by {})", join_numbers(&l.unmet_deps)));
            }
            line
        })
        .collect()
}

/// The readiness marker shown for a listed issue.
fn readiness(listed: &ListedIssue) -> &'static str {
    if listed.summary.status == IssueStatus::Done {
        "done"
    } else if listed.is_ready() {
        "ready"
    } else {
        "blocked"
    }
}

fn join_numbers(numbers: &[u32]) -> String {
    numbers
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Serialize `value` to pretty JSON and return it split into lines.
pub(super) fn json_lines<T: Serialize>(value: &T) -> Result<Vec<String>> {
    let text = serde_json::to_string_pretty(value)?;
    Ok(text.lines().map(str::to_string).collect())
}

//! Plain-text rendering of issue listings, shared by the CLI (`usagi issue
//! list` / `search` / `group`) and the TUI workspace screen's `issue` command.
//!
//! Both surfaces format an issue line, its readiness marker, and the progress
//! footer identically; keeping the formatting here (alongside the [`gantt`] and
//! [`dependency_tree`] renderers, which already return `Vec<String>`) means a
//! change to the layout cannot leave the two surfaces showing different shapes.
//!
//! [`gantt`]: super::gantt
//! [`dependency_tree`]: super::dependency_tree

use super::{IssueStats, ListedIssue};
use crate::domain::issue::IssueStatus;

/// The readiness marker shown for a listed issue: `done`, `ready`, or `blocked`.
pub fn readiness_marker(listed: &ListedIssue) -> &'static str {
    if listed.summary.status == IssueStatus::Done {
        "done"
    } else if listed.is_ready() {
        "ready"
    } else {
        "blocked"
    }
}

/// One aligned `#N status priority marker title` line for a listed issue, with a
/// trailing `(blocked by …)` when it has unmet dependencies.
pub fn list_line(listed: &ListedIssue) -> String {
    let mut line = format!(
        "#{:<3} {:<12} {:<6} {:<8} {}",
        listed.summary.number,
        listed.summary.status.as_str(),
        listed.summary.priority.as_str(),
        readiness_marker(listed),
        listed.summary.title,
    );
    if !listed.unmet_deps.is_empty() {
        line.push_str(&format!(
            "  (blocked by {})",
            join_numbers(&listed.unmet_deps)
        ));
    }
    line
}

/// A one-line progress summary: totals, completion, and readiness.
pub fn stats_line(stats: &IssueStats) -> String {
    format!(
        "{} issues · {} done ({}%) · {} ready  {}",
        stats.total,
        stats.done,
        stats.completion_percent(),
        stats.ready,
        stats.progress_bar(20),
    )
}

fn join_numbers(numbers: &[u32]) -> String {
    numbers
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::issue::{IssuePriority, IssueStatus, IssueSummary};
    use chrono::{TimeZone, Utc};

    fn listed(status: IssueStatus, unmet_deps: Vec<u32>) -> ListedIssue {
        let ts = Utc.with_ymd_and_hms(2026, 6, 21, 0, 0, 0).unwrap();
        ListedIssue {
            summary: IssueSummary {
                number: 7,
                title: "Wire it up".to_string(),
                status,
                priority: IssuePriority::High,
                labels: vec![],
                dependson: vec![],
                related: vec![],
                parent: None,
                milestone: None,
                file: "007-wire-it-up.md".to_string(),
                created_at: ts,
                updated_at: ts,
            },
            unmet_deps,
        }
    }

    #[test]
    fn marker_reflects_status_and_readiness() {
        assert_eq!(readiness_marker(&listed(IssueStatus::Done, vec![])), "done");
        assert_eq!(
            readiness_marker(&listed(IssueStatus::Todo, vec![])),
            "ready"
        );
        assert_eq!(
            readiness_marker(&listed(IssueStatus::Todo, vec![3])),
            "blocked"
        );
    }

    #[test]
    fn line_includes_the_blocked_by_suffix_only_when_blocked() {
        let ready = list_line(&listed(IssueStatus::Todo, vec![]));
        assert!(ready.starts_with("#7"));
        assert!(ready.contains("ready"));
        assert!(!ready.contains("blocked by"));

        let blocked = list_line(&listed(IssueStatus::Todo, vec![3, 4]));
        assert!(blocked.contains("(blocked by 3, 4)"));
    }

    #[test]
    fn stats_line_reports_totals_and_a_bar() {
        let listed_items = vec![
            listed(IssueStatus::Done, vec![]),
            listed(IssueStatus::Todo, vec![]),
        ];
        let line = stats_line(&IssueStats::from_listed(&listed_items));
        assert!(line.contains("2 issues · 1 done (50%) · 1 ready"));
        assert!(line.contains('['));
    }
}

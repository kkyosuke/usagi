//! Rendering of issues as an ASCII Gantt chart along a real-date axis.
//!
//! Each issue becomes one row whose bar spans its `created_at`→`updated_at`
//! across a single shared timeline, so reading top-to-bottom (rows are sorted by
//! start date) follows the order work actually began. Dependencies are layered on
//! as an auxiliary annotation — every row lists the issues it `dependson`, with a
//! `!` marking any that are not yet `done` (i.e. still blocking).

use chrono::{DateTime, Utc};

use super::ListedIssue;
use crate::domain::issue::IssueStatus;

/// Bar glyph for a `done` issue.
const DONE_GLYPH: char = '█';
/// Bar glyph for an `in-progress` issue.
const IN_PROGRESS_GLYPH: char = '▒';
/// Bar glyph for a `todo` issue.
const TODO_GLYPH: char = '░';

/// Render `items` as a Gantt chart sized to a `width`-column line budget.
///
/// The timeline runs from the earliest `created_at` to the latest `updated_at`
/// across all issues; each issue's bar covers its own `[created_at, updated_at]`
/// interval (always at least one cell, so even untouched issues are visible).
/// Returns an empty vector when there are no issues.
pub fn gantt(items: &[ListedIssue], width: usize) -> Vec<String> {
    if items.is_empty() {
        return Vec::new();
    }

    // Shared span across every issue's [created_at, updated_at].
    let span_start = items.iter().map(|i| i.summary.created_at).min().unwrap();
    let span_end = items.iter().map(|i| i.summary.updated_at).max().unwrap();
    let total_secs = (span_end - span_start).num_seconds().max(0) as u64;

    // Column budget: "#NNN │<bar>│ title …". The label holds the widest "#NNN".
    let max_number = items.iter().map(|i| i.summary.number).max().unwrap();
    let label_w = format!("#{max_number}").len();
    // Give the bar ~40% of the line, leaving the rest for the (clippable) title.
    let bar_w = (width * 2 / 5).clamp(8, 40);

    // Rows in start-date order, ties broken by number for a stable layout.
    let mut rows: Vec<&ListedIssue> = items.iter().collect();
    rows.sort_by_key(|i| (i.summary.created_at, i.summary.number));

    let mut out = Vec::with_capacity(rows.len() + 3);

    // Header: the date span and how long it covers.
    let span_days = (span_end - span_start).num_days();
    let span_label = if span_days > 0 {
        format!("{span_days} 日間")
    } else {
        "同日".to_string()
    };
    out.push(format!(
        "{} → {}  ({span_label})",
        span_start.format("%Y-%m-%d"),
        span_end.format("%Y-%m-%d"),
    ));
    out.push(format!(
        "凡例: {DONE_GLYPH} done  {IN_PROGRESS_GLYPH} in-progress  {TODO_GLYPH} todo   ←依存 (! = 未完了)"
    ));
    out.push(String::new());

    for row in rows {
        let s = &row.summary;
        let glyph = match s.status {
            IssueStatus::Done => DONE_GLYPH,
            IssueStatus::InProgress => IN_PROGRESS_GLYPH,
            IssueStatus::Todo => TODO_GLYPH,
        };
        let start_col = column(s.created_at, span_start, total_secs, bar_w);
        let end_col = column(s.updated_at, span_start, total_secs, bar_w).max(start_col);
        let bar: String = (0..bar_w)
            .map(|c| {
                if (start_col..=end_col).contains(&c) {
                    glyph
                } else {
                    ' '
                }
            })
            .collect();

        let deps = if s.dependson.is_empty() {
            String::new()
        } else {
            let parts: Vec<String> = s
                .dependson
                .iter()
                .map(|d| {
                    if row.unmet_deps.contains(d) {
                        format!("{d}!")
                    } else {
                        d.to_string()
                    }
                })
                .collect();
            format!("  ←{}", parts.join(","))
        };

        let id = format!("#{}", s.number);
        out.push(format!("{id:<label_w$} │{bar}│ {}{deps}", s.title));
    }

    out
}

/// Map a timestamp to a bar column in `0..bar_w` (`bar_w` is always ≥ 8). A
/// zero-length span (every issue touched at the same instant) collapses to the
/// first column.
fn column(at: DateTime<Utc>, span_start: DateTime<Utc>, total_secs: u64, bar_w: usize) -> usize {
    if total_secs == 0 {
        return 0;
    }
    let secs = (at - span_start).num_seconds().max(0) as u64;
    let last = (bar_w - 1) as u128;
    ((secs as u128 * last) / total_secs as u128) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::issue::{IssuePriority, IssueSummary};
    use chrono::TimeZone;

    fn summary(number: u32, status: IssueStatus, day: u32, end_day: u32) -> IssueSummary {
        IssueSummary {
            number,
            title: format!("task {number}"),
            status,
            priority: IssuePriority::Medium,
            labels: vec![],
            dependson: vec![],
            related: vec![],
            parent: None,
            milestone: None,
            file: format!("{number:03}-task.md"),
            created_at: Utc.with_ymd_and_hms(2026, 6, day, 0, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 6, end_day, 0, 0, 0).unwrap(),
        }
    }

    fn listed(summary: IssueSummary, unmet_deps: Vec<u32>) -> ListedIssue {
        ListedIssue {
            summary,
            unmet_deps,
        }
    }

    #[test]
    fn empty_input_renders_nothing() {
        assert!(gantt(&[], 60).is_empty());
    }

    #[test]
    fn header_reports_the_span_and_legend() {
        let items = vec![
            listed(summary(1, IssueStatus::Done, 10, 12), vec![]),
            listed(summary(2, IssueStatus::Todo, 14, 19), vec![]),
        ];
        let lines = gantt(&items, 60);
        assert!(lines[0].contains("2026-06-10 → 2026-06-19"));
        assert!(lines[0].contains("9 日間"));
        assert!(lines[1].contains("done"));
        assert!(lines[1].contains("未完了"));
    }

    #[test]
    fn rows_sort_by_start_date_and_use_status_glyphs() {
        let items = vec![
            // Defined out of order; should render #2 before #1 by start date.
            listed(summary(1, IssueStatus::Todo, 18, 19), vec![]),
            listed(summary(2, IssueStatus::Done, 10, 11), vec![]),
        ];
        let lines = gantt(&items, 60);
        let rows: Vec<&String> = lines.iter().skip(3).collect();
        assert!(rows[0].starts_with("#2"));
        assert!(rows[0].contains(DONE_GLYPH));
        assert!(rows[1].starts_with("#1"));
        assert!(rows[1].contains(TODO_GLYPH));
    }

    #[test]
    fn bar_position_reflects_the_interval() {
        // #1 spans the whole window; #2 only the final day.
        let items = vec![
            listed(summary(1, IssueStatus::InProgress, 1, 11), vec![]),
            listed(summary(2, IssueStatus::Todo, 10, 11), vec![]),
        ];
        let lines = gantt(&items, 60);
        let full = lines.iter().find(|l| l.starts_with("#1")).unwrap();
        let tail = lines.iter().find(|l| l.starts_with("#2")).unwrap();
        // #1 fills from the first cell; #2's bar starts later, so it has leading
        // blank cells inside the brackets.
        let bar1 = full.split('│').nth(1).unwrap();
        let bar2 = tail.split('│').nth(1).unwrap();
        assert!(bar1.starts_with(IN_PROGRESS_GLYPH));
        assert!(bar2.starts_with(' '));
        assert!(bar2.contains(TODO_GLYPH));
    }

    #[test]
    fn dependencies_are_annotated_with_unmet_markers() {
        let items = vec![
            listed(summary(1, IssueStatus::Done, 10, 11), vec![]),
            // depends on #1 (done) and #2 (unmet).
            listed(summary(3, IssueStatus::Todo, 12, 13), vec![2]),
        ];
        // Give #3 two dependencies to exercise the join.
        let mut items = items;
        items[1].summary.dependson = vec![1, 2];
        let lines = gantt(&items, 60);
        let row = lines.iter().find(|l| l.starts_with("#3")).unwrap();
        assert!(row.contains("←1,2!"));
    }

    #[test]
    fn single_instant_span_collapses_to_one_cell() {
        let items = vec![listed(summary(1, IssueStatus::Todo, 14, 14), vec![])];
        let lines = gantt(&items, 60);
        assert!(lines[0].contains("同日"));
        let row = lines.iter().find(|l| l.starts_with("#1")).unwrap();
        let bar = row.split('│').nth(1).unwrap();
        assert_eq!(bar.chars().filter(|c| *c == TODO_GLYPH).count(), 1);
    }
}

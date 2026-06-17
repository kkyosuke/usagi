//! Aggregate statistics and grouping for issue listings.

use std::fmt;
use std::str::FromStr;

use super::ListedIssue;
use crate::domain::issue::{IssuePriority, IssueStatus, ParseIssueError};

/// Aggregate counts over a set of listed issues, used for progress summaries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IssueStats {
    pub total: usize,
    pub todo: usize,
    pub in_progress: usize,
    pub done: usize,
    /// Issues that are ready to start (not done, all dependencies done).
    pub ready: usize,
}

impl IssueStats {
    /// Tally the status breakdown and readiness of `items`.
    pub fn from_listed(items: &[ListedIssue]) -> Self {
        let mut stats = IssueStats::default();
        for item in items {
            stats.total += 1;
            match item.summary.status {
                IssueStatus::Todo => stats.todo += 1,
                IssueStatus::InProgress => stats.in_progress += 1,
                IssueStatus::Done => stats.done += 1,
            }
            if item.is_ready() {
                stats.ready += 1;
            }
        }
        stats
    }

    /// Completion as a whole-number percentage (0 when there are no issues).
    pub fn completion_percent(&self) -> u32 {
        (self.done * 100).checked_div(self.total).unwrap_or(0) as u32
    }

    /// A fixed-width `[####----]` bar reflecting completion.
    pub fn progress_bar(&self, width: usize) -> String {
        let filled = (self.done * width).checked_div(self.total).unwrap_or(0);
        let mut bar = String::with_capacity(width + 2);
        bar.push('[');
        for i in 0..width {
            bar.push(if i < filled { '#' } else { '-' });
        }
        bar.push(']');
        bar
    }
}

/// The axis a listing can be grouped by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    Status,
    Priority,
    Milestone,
    Parent,
}

impl fmt::Display for GroupBy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            GroupBy::Status => "status",
            GroupBy::Priority => "priority",
            GroupBy::Milestone => "milestone",
            GroupBy::Parent => "parent",
        })
    }
}

impl FromStr for GroupBy {
    type Err = ParseIssueError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "status" => Ok(GroupBy::Status),
            "priority" => Ok(GroupBy::Priority),
            "milestone" => Ok(GroupBy::Milestone),
            "parent" => Ok(GroupBy::Parent),
            other => Err(ParseIssueError(format!("invalid group-by: {other:?}"))),
        }
    }
}

/// Partition `items` into labelled groups along `axis`. Groups come back in a
/// stable, meaningful order (status/priority follow their lifecycle order;
/// milestone/parent sort with a trailing "(none)" bucket) and empty groups are
/// omitted.
pub fn group(items: Vec<ListedIssue>, axis: GroupBy) -> Vec<(String, Vec<ListedIssue>)> {
    // Assign each item a (sort key, label) pair, then bucket preserving order.
    let mut buckets: Vec<(String, String, Vec<ListedIssue>)> = Vec::new();
    for item in items {
        let (key, label) = group_key(&item, axis);
        match buckets.iter_mut().find(|(k, _, _)| *k == key) {
            Some((_, _, group)) => group.push(item),
            None => buckets.push((key, label, vec![item])),
        }
    }
    buckets.sort_by(|a, b| a.0.cmp(&b.0));
    buckets
        .into_iter()
        .map(|(_, label, group)| (label, group))
        .collect()
}

/// The sort key and display label for `item` under `axis`. The sort key encodes
/// the desired ordering; "(none)" buckets sort last via a `~` prefix.
fn group_key(item: &ListedIssue, axis: GroupBy) -> (String, String) {
    let s = &item.summary;
    match axis {
        GroupBy::Status => {
            let rank = match s.status {
                IssueStatus::Todo => 0,
                IssueStatus::InProgress => 1,
                IssueStatus::Done => 2,
            };
            (format!("{rank}"), s.status.to_string())
        }
        GroupBy::Priority => {
            let rank = match s.priority {
                IssuePriority::High => 0,
                IssuePriority::Medium => 1,
                IssuePriority::Low => 2,
            };
            (format!("{rank}"), s.priority.to_string())
        }
        GroupBy::Milestone => match &s.milestone {
            Some(m) => (format!("0{m}"), m.clone()),
            None => ("~".to_string(), "(no milestone)".to_string()),
        },
        GroupBy::Parent => match s.parent {
            // Zero-pad so numeric parents sort numerically as strings.
            Some(p) => (format!("0{p:08}"), format!("#{p}")),
            None => ("~".to_string(), "(no parent)".to_string()),
        },
    }
}

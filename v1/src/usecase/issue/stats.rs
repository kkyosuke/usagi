//! Aggregate statistics and grouping for issue listings.

use std::collections::BTreeMap;

use super::ListedIssue;
use crate::domain::frontmatter::str_enum;
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

    /// Fold `other`'s counts into these. Lets a grouped listing accumulate the
    /// overall total from its per-group tallies in one pass, instead of scanning
    /// the whole listing again just for the overall.
    pub fn merge(&mut self, other: &IssueStats) {
        self.total += other.total;
        self.todo += other.todo;
        self.in_progress += other.in_progress;
        self.done += other.done;
        self.ready += other.ready;
    }

    /// Completion as a whole-number percentage (0 when there are no issues).
    pub fn completion_percent(&self) -> u32 {
        if self.total == 0 {
            return 0;
        }
        // Compute in `u128` so a large `done` can never overflow the `* 100`
        // before the divide. Real stats have `done <= total`, but saturate the
        // public result defensively if a hand-built value violates that.
        ((self.done as u128 * 100) / self.total as u128).min(u32::MAX as u128) as u32
    }

    /// A fixed-width `[####----]` bar reflecting completion.
    pub fn progress_bar(&self, width: usize) -> String {
        // Compute in `u128` so a large `done * width` can never overflow before
        // the divide; the result is bounded by `width`, so it fits back in
        // `usize`.
        let filled = if self.total == 0 {
            0
        } else {
            ((self.done as u128 * width as u128) / self.total as u128).min(width as u128) as usize
        };
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

str_enum!(GroupBy, ParseIssueError, "group-by", {
    Status => "status",
    Priority => "priority",
    Milestone => "milestone",
    Parent => "parent",
});

/// Partition `items` into labelled groups along `axis`. Groups come back in a
/// stable, meaningful order (status/priority follow their lifecycle order;
/// milestone/parent sort with a trailing "(none)" bucket) and empty groups are
/// omitted.
pub fn group(items: Vec<ListedIssue>, axis: GroupBy) -> Vec<(String, Vec<ListedIssue>)> {
    // Bucket by each item's sort key. A `BTreeMap` keeps the buckets ordered by
    // key as they are inserted — so the result needs no separate sort — and looks
    // each key up in O(log g) rather than the O(g) linear scan a `Vec` would do
    // per item (which degrades toward O(n²) when grouping by milestone/parent,
    // where nearly every issue can fall in its own bucket).
    let mut buckets: BTreeMap<String, (String, Vec<ListedIssue>)> = BTreeMap::new();
    for item in items {
        let (key, label) = group_key(&item, axis);
        buckets
            .entry(key)
            .or_insert_with(|| (label, Vec::new()))
            .1
            .push(item);
    }
    buckets.into_values().collect()
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
            // Milestones are free-text, so they sort lexically (the `0` prefix
            // only keeps every named milestone ahead of the `~` no-milestone
            // bucket). Unlike `Parent` there is no numeric form to zero-pad, so
            // `v10` sorts before `v2` — acceptable for arbitrary labels.
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

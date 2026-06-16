//! Business logic for task issues: create, read, update, delete, list and
//! search issues stored under `<repo>/.usagi/issues/`.
//!
//! Listing and searching annotate each issue with its dependency *readiness* —
//! whether every issue it `dependson` is already `done` — so callers can surface
//! the tasks that are actually ready to pick up.

use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use std::str::FromStr;

use anyhow::Result;
use chrono::Utc;

use crate::domain::issue::{Issue, IssuePriority, IssueStatus, IssueSummary, ParseIssueError};
use crate::infrastructure::issue_store::IssueStore;

/// Fields needed to open a new issue. The number and timestamps are assigned by
/// [`create`].
pub struct NewIssue {
    pub title: String,
    pub priority: IssuePriority,
    pub labels: Vec<String>,
    pub dependson: Vec<u32>,
    pub related: Vec<u32>,
    pub parent: Option<u32>,
    pub milestone: Option<String>,
    pub body: String,
}

/// A partial update to an existing issue: every `Some` field is applied, every
/// `None` field is left unchanged.
#[derive(Default)]
pub struct IssueChanges {
    pub title: Option<String>,
    pub status: Option<IssueStatus>,
    pub priority: Option<IssuePriority>,
    pub labels: Option<Vec<String>>,
    pub dependson: Option<Vec<u32>>,
    pub related: Option<Vec<u32>>,
    /// Outer `None` leaves the parent unchanged; `Some(None)` clears it;
    /// `Some(Some(n))` sets it.
    pub parent: Option<Option<u32>>,
    /// Outer `None` leaves the milestone unchanged; `Some(None)` clears it;
    /// `Some(Some(name))` sets it.
    pub milestone: Option<Option<String>>,
    pub body: Option<String>,
}

impl IssueChanges {
    /// Whether this update would change anything.
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.status.is_none()
            && self.priority.is_none()
            && self.labels.is_none()
            && self.dependson.is_none()
            && self.related.is_none()
            && self.parent.is_none()
            && self.milestone.is_none()
            && self.body.is_none()
    }
}

/// Filters applied to listings and searches. An unset field matches everything.
#[derive(Default)]
pub struct IssueFilter {
    pub status: Option<IssueStatus>,
    pub priority: Option<IssuePriority>,
    pub label: Option<String>,
    pub parent: Option<u32>,
    pub milestone: Option<String>,
    /// Keep only issues that are ready to start (not done, all deps done).
    pub ready_only: bool,
}

impl IssueFilter {
    fn matches(&self, listed: &ListedIssue) -> bool {
        let s = &listed.summary;
        self.status.is_none_or(|status| s.status == status)
            && self.priority.is_none_or(|priority| s.priority == priority)
            && self
                .label
                .as_ref()
                .is_none_or(|label| s.labels.iter().any(|l| l == label))
            && self.parent.is_none_or(|parent| s.parent == Some(parent))
            && self
                .milestone
                .as_ref()
                .is_none_or(|milestone| s.milestone.as_deref() == Some(milestone.as_str()))
            && (!self.ready_only || listed.is_ready())
    }
}

/// An issue summary annotated with dependency readiness.
pub struct ListedIssue {
    pub summary: IssueSummary,
    /// Numbers this issue depends on that are not yet `done` (including any that
    /// no longer exist).
    pub unmet_deps: Vec<u32>,
}

impl ListedIssue {
    /// Ready to start: not already done, with every dependency done.
    pub fn is_ready(&self) -> bool {
        self.summary.status != IssueStatus::Done && self.unmet_deps.is_empty()
    }
}

/// Create a new issue, assigning the next number and timestamps. Returns the
/// stored issue.
pub fn create(repo_root: &Path, new: NewIssue) -> Result<Issue> {
    let store = IssueStore::new(repo_root);
    let now = Utc::now();
    let issue = Issue {
        number: store.max_number()? + 1,
        title: new.title,
        status: IssueStatus::Todo,
        priority: new.priority,
        labels: new.labels,
        dependson: new.dependson,
        related: new.related,
        parent: new.parent,
        milestone: new.milestone,
        created_at: now,
        updated_at: now,
        body: new.body,
    };
    store.write(&issue)?;
    Ok(issue)
}

/// Fetch a single issue by number.
pub fn get(repo_root: &Path, number: u32) -> Result<Option<Issue>> {
    IssueStore::new(repo_root).read(number)
}

/// List issues matching `filter`, annotated with dependency readiness.
pub fn list(repo_root: &Path, filter: &IssueFilter) -> Result<Vec<ListedIssue>> {
    let all = IssueStore::new(repo_root).summaries()?;
    let done = done_numbers(all.iter().map(|s| (s.number, s.status)));
    Ok(annotate(all, &done)
        .into_iter()
        .filter(|l| filter.matches(l))
        .collect())
}

/// Full-text search issue titles and bodies (case-insensitive), then apply
/// `filter`. Results are annotated with dependency readiness.
pub fn search(repo_root: &Path, query: &str, filter: &IssueFilter) -> Result<Vec<ListedIssue>> {
    let issues = IssueStore::new(repo_root).scan()?;
    let done = done_numbers(issues.iter().map(|i| (i.number, i.status)));
    let needle = query.to_ascii_lowercase();
    let needle_bytes = needle.as_bytes();
    let matched: Vec<IssueSummary> = issues
        .into_iter()
        .filter(|i| {
            if needle_bytes.is_empty() {
                return true;
            }
            let title_match = i
                .title
                .as_bytes()
                .windows(needle_bytes.len())
                .any(|w| w.eq_ignore_ascii_case(needle_bytes));
            let body_match = i
                .body
                .as_bytes()
                .windows(needle_bytes.len())
                .any(|w| w.eq_ignore_ascii_case(needle_bytes));
            title_match || body_match
        })
        .map(|i| i.summary())
        .collect();
    Ok(annotate(matched, &done)
        .into_iter()
        .filter(|l| filter.matches(l))
        .collect())
}

/// Apply `changes` to the issue with `number`. Returns the updated issue, or
/// `None` if no such issue exists.
pub fn update(repo_root: &Path, number: u32, changes: IssueChanges) -> Result<Option<Issue>> {
    let store = IssueStore::new(repo_root);
    let Some(mut issue) = store.read(number)? else {
        return Ok(None);
    };
    if let Some(title) = changes.title {
        issue.title = title;
    }
    if let Some(status) = changes.status {
        issue.status = status;
    }
    if let Some(priority) = changes.priority {
        issue.priority = priority;
    }
    if let Some(labels) = changes.labels {
        issue.labels = labels;
    }
    if let Some(dependson) = changes.dependson {
        issue.dependson = dependson;
    }
    if let Some(related) = changes.related {
        issue.related = related;
    }
    if let Some(parent) = changes.parent {
        issue.parent = parent;
    }
    if let Some(milestone) = changes.milestone {
        issue.milestone = milestone;
    }
    if let Some(body) = changes.body {
        issue.body = body;
    }
    issue.updated_at = Utc::now();
    store.write(&issue)?;
    Ok(Some(issue))
}

/// Delete the issue with `number`, returning whether it existed.
pub fn delete(repo_root: &Path, number: u32) -> Result<bool> {
    IssueStore::new(repo_root).remove(number)
}

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

/// Render a dependency forest as indented ASCII lines: each issue appears under
/// the issues it `dependson`, so reading top-to-bottom follows the order work
/// can be picked up. Roots are issues with no dependencies; issues reached again
/// (diamonds or cycles) are shown once with a `↑` marker and not re-expanded.
pub fn dependency_tree(items: &[ListedIssue]) -> Vec<String> {
    use std::collections::BTreeMap;

    let by_number: BTreeMap<u32, &ListedIssue> =
        items.iter().map(|i| (i.summary.number, i)).collect();
    // children[d] = issues that depend on d, kept sorted by number.
    let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for item in items {
        for dep in &item.summary.dependson {
            children.entry(*dep).or_default().push(item.summary.number);
        }
    }

    let mut visited: HashSet<u32> = HashSet::new();
    let mut out = Vec::new();

    // Start from: dependency targets that don't exist as issues (so their
    // dependents are still shown), then roots (no dependencies), then every
    // remaining node so nothing is dropped amid orphaned deps or cycles.
    let mut starts: Vec<u32> = children
        .keys()
        .copied()
        .filter(|d| !by_number.contains_key(d))
        .collect();
    starts.extend(
        items
            .iter()
            .filter(|i| i.summary.dependson.is_empty())
            .map(|i| i.summary.number),
    );
    starts.extend(items.iter().map(|i| i.summary.number));

    for num in starts {
        if visited.contains(&num) {
            continue;
        }
        out.push(node_label(num, &by_number, &mut visited));
        walk_children(num, &children, &by_number, "", &mut visited, &mut out);
    }
    out
}

fn walk_children(
    num: u32,
    children: &std::collections::BTreeMap<u32, Vec<u32>>,
    by_number: &std::collections::BTreeMap<u32, &ListedIssue>,
    prefix: &str,
    visited: &mut HashSet<u32>,
    out: &mut Vec<String>,
) {
    let Some(kids) = children.get(&num) else {
        return;
    };
    let last_index = kids.len() - 1;
    for (i, &child) in kids.iter().enumerate() {
        let is_last = i == last_index;
        let branch = if is_last { "└─ " } else { "├─ " };
        let already = visited.contains(&child);
        out.push(format!(
            "{prefix}{branch}{}",
            node_label(child, by_number, visited)
        ));
        if !already {
            let extension = if is_last { "   " } else { "│  " };
            walk_children(
                child,
                children,
                by_number,
                &format!("{prefix}{extension}"),
                visited,
                out,
            );
        }
    }
}

/// One node's label, marking the first/repeat visit. Records the visit.
fn node_label(
    num: u32,
    by_number: &std::collections::BTreeMap<u32, &ListedIssue>,
    visited: &mut HashSet<u32>,
) -> String {
    let repeat = !visited.insert(num);
    match by_number.get(&num) {
        Some(item) => {
            let mark = if repeat { " ↑" } else { "" };
            format!(
                "#{} {} [{}]{mark}",
                item.summary.number, item.summary.title, item.summary.status
            )
        }
        // A dependency that points at a non-existent issue.
        None => format!("#{num} (missing)"),
    }
}

/// Annotate an in-memory set of issues with dependency readiness, without
/// touching disk. Used by callers (e.g. the TUI) that already hold the issues.
pub fn annotate_all(issues: &[Issue]) -> Vec<ListedIssue> {
    let done = done_numbers(issues.iter().map(|i| (i.number, i.status)));
    annotate(issues.iter().map(Issue::summary).collect(), &done)
}

/// Collect the set of issue numbers whose status is `done`.
fn done_numbers(items: impl Iterator<Item = (u32, IssueStatus)>) -> HashSet<u32> {
    items
        .filter(|(_, status)| *status == IssueStatus::Done)
        .map(|(number, _)| number)
        .collect()
}

/// Pair each summary with the dependency numbers that are not yet done.
fn annotate(summaries: Vec<IssueSummary>, done: &HashSet<u32>) -> Vec<ListedIssue> {
    summaries
        .into_iter()
        .map(|summary| {
            let unmet_deps = summary
                .dependson
                .iter()
                .copied()
                .filter(|d| !done.contains(d))
                .collect();
            ListedIssue {
                summary,
                unmet_deps,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    /// Build a listed issue from the fields the visualization helpers read,
    /// without touching disk.
    fn listed(
        number: u32,
        status: IssueStatus,
        dependson: Vec<u32>,
        unmet_deps: Vec<u32>,
        parent: Option<u32>,
        milestone: Option<&str>,
    ) -> ListedIssue {
        let ts = Utc.with_ymd_and_hms(2026, 6, 14, 0, 0, 0).unwrap();
        ListedIssue {
            summary: IssueSummary {
                number,
                title: format!("issue {number}"),
                status,
                priority: IssuePriority::Medium,
                labels: vec![],
                dependson,
                related: vec![],
                parent,
                milestone: milestone.map(str::to_string),
                file: format!("{number:03}-issue.md"),
                created_at: ts,
                updated_at: ts,
            },
            unmet_deps,
        }
    }

    #[test]
    fn stats_tally_status_readiness_and_progress() {
        let items = vec![
            listed(1, IssueStatus::Done, vec![], vec![], None, None),
            listed(2, IssueStatus::InProgress, vec![], vec![], None, None),
            listed(3, IssueStatus::Todo, vec![], vec![], None, None),
            listed(4, IssueStatus::Todo, vec![3], vec![3], None, None),
        ];
        let stats = IssueStats::from_listed(&items);
        assert_eq!(stats.total, 4);
        assert_eq!(stats.done, 1);
        assert_eq!(stats.in_progress, 1);
        assert_eq!(stats.todo, 2);
        // Ready = not done with all deps met: #2 (in-progress) and #3 (todo).
        // #4 is blocked by #3; #1 is done.
        assert_eq!(stats.ready, 2);
        assert_eq!(stats.completion_percent(), 25);
        assert_eq!(stats.progress_bar(8), "[##------]");
    }

    #[test]
    fn empty_stats_have_zero_completion_and_empty_bar() {
        let stats = IssueStats::from_listed(&[]);
        assert_eq!(stats.completion_percent(), 0);
        assert_eq!(stats.progress_bar(4), "[----]");
    }

    #[test]
    fn group_by_round_trips_through_string() {
        for g in [
            GroupBy::Status,
            GroupBy::Priority,
            GroupBy::Milestone,
            GroupBy::Parent,
        ] {
            assert_eq!(g.to_string().parse::<GroupBy>().unwrap(), g);
        }
        assert!("nope".parse::<GroupBy>().is_err());
    }

    #[test]
    fn group_orders_status_and_keeps_lifecycle_order() {
        let items = vec![
            listed(1, IssueStatus::Done, vec![], vec![], None, None),
            listed(2, IssueStatus::Todo, vec![], vec![], None, None),
            listed(3, IssueStatus::InProgress, vec![], vec![], None, None),
        ];
        let groups = group(items, GroupBy::Status);
        let labels: Vec<&str> = groups.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, vec!["todo", "in-progress", "done"]);
    }

    #[test]
    fn group_by_priority_orders_and_merges_same_bucket() {
        let mut high = listed(1, IssueStatus::Todo, vec![], vec![], None, None);
        high.summary.priority = IssuePriority::High;
        let mut low = listed(2, IssueStatus::Todo, vec![], vec![], None, None);
        low.summary.priority = IssuePriority::Low;
        // Two mediums land in the same bucket, exercising the merge path.
        let med_a = listed(3, IssueStatus::Todo, vec![], vec![], None, None);
        let med_b = listed(4, IssueStatus::Todo, vec![], vec![], None, None);

        let groups = group(vec![high, low, med_a, med_b], GroupBy::Priority);
        let labels: Vec<&str> = groups.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, vec!["high", "medium", "low"]);
        let medium = groups.iter().find(|(l, _)| l == "medium").unwrap();
        assert_eq!(medium.1.len(), 2);
    }

    #[test]
    fn group_by_milestone_and_parent_put_none_last() {
        let items = vec![
            listed(1, IssueStatus::Todo, vec![], vec![], None, Some("v2")),
            listed(2, IssueStatus::Todo, vec![], vec![], None, Some("v1")),
            listed(3, IssueStatus::Todo, vec![], vec![], None, None),
        ];
        let groups = group(items, GroupBy::Milestone);
        let labels: Vec<&str> = groups.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, vec!["v1", "v2", "(no milestone)"]);

        let items = vec![
            listed(1, IssueStatus::Todo, vec![], vec![], Some(10), None),
            listed(2, IssueStatus::Todo, vec![], vec![], Some(2), None),
            listed(3, IssueStatus::Todo, vec![], vec![], None, None),
        ];
        let groups = group(items, GroupBy::Parent);
        let labels: Vec<&str> = groups.iter().map(|(l, _)| l.as_str()).collect();
        // #2 sorts before #10 numerically, "(no parent)" last.
        assert_eq!(labels, vec!["#2", "#10", "(no parent)"]);
    }

    #[test]
    fn dependency_tree_nests_dependents_under_dependencies() {
        // #1 root; #2 and #3 depend on #1; #4 depends on #2.
        let items = vec![
            listed(1, IssueStatus::Done, vec![], vec![], None, None),
            listed(2, IssueStatus::Todo, vec![1], vec![], None, None),
            listed(3, IssueStatus::Todo, vec![1], vec![], None, None),
            listed(4, IssueStatus::Todo, vec![2], vec![], None, None),
        ];
        let lines = dependency_tree(&items);
        assert_eq!(lines[0], "#1 issue 1 [done]");
        assert!(lines[1].contains("├─ #2 issue 2 [todo]"));
        assert!(lines.iter().any(|l| l.contains("└─ #4 issue 4 [todo]")));
        assert!(lines.iter().any(|l| l.contains("└─ #3 issue 3 [todo]")));
    }

    #[test]
    fn dependency_tree_marks_repeats_and_handles_cycles_and_missing() {
        // A diamond: #4 depends on both #2 and #3, which both depend on #1.
        let diamond = vec![
            listed(1, IssueStatus::Todo, vec![], vec![], None, None),
            listed(2, IssueStatus::Todo, vec![1], vec![], None, None),
            listed(3, IssueStatus::Todo, vec![1], vec![], None, None),
            listed(4, IssueStatus::Todo, vec![2, 3], vec![], None, None),
        ];
        let lines = dependency_tree(&diamond);
        // #4 appears under both #2 and #3; one of them carries the ↑ repeat mark.
        assert!(lines.iter().filter(|l| l.contains("#4 issue 4")).count() >= 2);
        assert!(lines.iter().any(|l| l.contains('↑')));

        // A pure cycle (#1↔#2) still terminates and shows both nodes.
        let cycle = vec![
            listed(1, IssueStatus::Todo, vec![2], vec![], None, None),
            listed(2, IssueStatus::Todo, vec![1], vec![], None, None),
        ];
        let lines = dependency_tree(&cycle);
        assert!(lines.iter().any(|l| l.contains("#1 issue 1")));
        assert!(lines.iter().any(|l| l.contains("#2 issue 2")));

        // A dependency on a non-existent issue is shown as missing.
        let orphan = vec![listed(1, IssueStatus::Todo, vec![99], vec![99], None, None)];
        let lines = dependency_tree(&orphan);
        assert!(lines.iter().any(|l| l.contains("#99 (missing)")));
    }

    fn new_issue(title: &str) -> NewIssue {
        NewIssue {
            title: title.to_string(),
            priority: IssuePriority::Medium,
            labels: vec![],
            dependson: vec![],
            related: vec![],
            parent: None,
            milestone: None,
            body: String::new(),
        }
    }

    #[test]
    fn create_assigns_increasing_numbers_and_defaults_to_todo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();

        let first = create(repo, new_issue("First")).unwrap();
        let second = create(repo, new_issue("Second")).unwrap();

        assert_eq!(first.number, 1);
        assert_eq!(second.number, 2);
        assert_eq!(first.status, IssueStatus::Todo);
        assert_eq!(get(repo, 1).unwrap().unwrap().title, "First");
        assert!(get(repo, 99).unwrap().is_none());
    }

    #[test]
    fn update_applies_only_set_fields_and_touches_updated_at() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let created = create(repo, new_issue("Title")).unwrap();

        let updated = update(
            repo,
            1,
            IssueChanges {
                status: Some(IssueStatus::Done),
                body: Some("done now".to_string()),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();

        assert_eq!(updated.status, IssueStatus::Done);
        assert_eq!(updated.body, "done now");
        // Untouched fields are preserved.
        assert_eq!(updated.title, "Title");
        assert_eq!(updated.priority, created.priority);
        assert!(updated.updated_at >= created.updated_at);
    }

    #[test]
    fn update_can_change_title_priority_labels_and_deps() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        create(repo, new_issue("Old")).unwrap();

        let updated = update(
            repo,
            1,
            IssueChanges {
                title: Some("New".to_string()),
                priority: Some(IssuePriority::High),
                labels: Some(vec!["cli".to_string()]),
                dependson: Some(vec![2]),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();

        assert_eq!(updated.title, "New");
        assert_eq!(updated.priority, IssuePriority::High);
        assert_eq!(updated.labels, vec!["cli"]);
        assert_eq!(updated.dependson, vec![2]);
    }

    #[test]
    fn update_returns_none_for_a_missing_issue() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(update(tmp.path(), 1, IssueChanges::default())
            .unwrap()
            .is_none());
    }

    #[test]
    fn delete_reports_whether_the_issue_existed() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        create(repo, new_issue("Doomed")).unwrap();

        assert!(delete(repo, 1).unwrap());
        assert!(!delete(repo, 1).unwrap());
    }

    #[test]
    fn changes_is_empty_detects_no_op_updates() {
        assert!(IssueChanges::default().is_empty());
        assert!(!IssueChanges {
            status: Some(IssueStatus::Done),
            ..Default::default()
        }
        .is_empty());
        // The relation fields also count as changes.
        assert!(!IssueChanges {
            parent: Some(Some(1)),
            ..Default::default()
        }
        .is_empty());
        assert!(!IssueChanges {
            milestone: Some(None),
            ..Default::default()
        }
        .is_empty());
        assert!(!IssueChanges {
            related: Some(vec![2]),
            ..Default::default()
        }
        .is_empty());
    }

    #[test]
    fn create_persists_relations_and_milestone() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let created = create(
            repo,
            NewIssue {
                related: vec![3],
                parent: Some(2),
                milestone: Some("v1".to_string()),
                ..new_issue("child")
            },
        )
        .unwrap();
        assert_eq!(created.related, vec![3]);
        assert_eq!(created.parent, Some(2));
        assert_eq!(created.milestone, Some("v1".to_string()));
    }

    #[test]
    fn update_sets_and_clears_parent_and_milestone() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        create(
            repo,
            NewIssue {
                parent: Some(2),
                milestone: Some("v1".to_string()),
                ..new_issue("task")
            },
        )
        .unwrap();

        // Setting replaces; related is replaced wholesale like dependson.
        let set = update(
            repo,
            1,
            IssueChanges {
                parent: Some(Some(5)),
                milestone: Some(Some("v2".to_string())),
                related: Some(vec![9]),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(set.parent, Some(5));
        assert_eq!(set.milestone, Some("v2".to_string()));
        assert_eq!(set.related, vec![9]);

        // An outer Some(None) clears the optional field.
        let cleared = update(
            repo,
            1,
            IssueChanges {
                parent: Some(None),
                milestone: Some(None),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(cleared.parent, None);
        assert_eq!(cleared.milestone, None);
        // An outer None leaves the field untouched.
        assert_eq!(cleared.related, vec![9]);
    }

    #[test]
    fn list_filters_by_parent_and_milestone() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        create(repo, new_issue("epic")).unwrap();
        create(
            repo,
            NewIssue {
                parent: Some(1),
                milestone: Some("v1".to_string()),
                ..new_issue("child a")
            },
        )
        .unwrap();
        create(
            repo,
            NewIssue {
                parent: Some(1),
                milestone: Some("v2".to_string()),
                ..new_issue("child b")
            },
        )
        .unwrap();

        let by_parent = list(
            repo,
            &IssueFilter {
                parent: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(by_parent.len(), 2);

        let by_milestone = list(
            repo,
            &IssueFilter {
                milestone: Some("v1".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(by_milestone.len(), 1);
        assert_eq!(by_milestone[0].summary.number, 2);
    }

    #[test]
    fn list_annotates_readiness_from_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        // #1 todo, #2 depends on #1, #3 depends on #1.
        create(repo, new_issue("base")).unwrap();
        create(
            repo,
            NewIssue {
                dependson: vec![1],
                ..new_issue("blocked")
            },
        )
        .unwrap();

        let listed = list(repo, &IssueFilter::default()).unwrap();
        let blocked = listed.iter().find(|l| l.summary.number == 2).unwrap();
        // #1 is not done yet, so #2 is blocked.
        assert_eq!(blocked.unmet_deps, vec![1]);
        assert!(!blocked.is_ready());
        // #1 has no deps, so it is ready.
        let base = listed.iter().find(|l| l.summary.number == 1).unwrap();
        assert!(base.is_ready());

        // Mark #1 done: #2 becomes ready.
        update(
            repo,
            1,
            IssueChanges {
                status: Some(IssueStatus::Done),
                ..Default::default()
            },
        )
        .unwrap();
        let listed = list(repo, &IssueFilter::default()).unwrap();
        let blocked = listed.iter().find(|l| l.summary.number == 2).unwrap();
        assert!(blocked.unmet_deps.is_empty());
        assert!(blocked.is_ready());
        // A done issue is never "ready".
        let base = listed.iter().find(|l| l.summary.number == 1).unwrap();
        assert!(!base.is_ready());
    }

    #[test]
    fn nonexistent_dependency_counts_as_unmet() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        create(
            repo,
            NewIssue {
                dependson: vec![999],
                ..new_issue("orphan dep")
            },
        )
        .unwrap();

        let listed = list(repo, &IssueFilter::default()).unwrap();
        assert_eq!(listed[0].unmet_deps, vec![999]);
        assert!(!listed[0].is_ready());
    }

    #[test]
    fn list_filters_by_status_priority_label_and_readiness() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        create(
            repo,
            NewIssue {
                priority: IssuePriority::High,
                labels: vec!["cli".to_string()],
                ..new_issue("a")
            },
        )
        .unwrap();
        create(
            repo,
            NewIssue {
                dependson: vec![1],
                ..new_issue("b")
            },
        )
        .unwrap();

        // Priority filter.
        let high = list(
            repo,
            &IssueFilter {
                priority: Some(IssuePriority::High),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(high.len(), 1);
        assert_eq!(high[0].summary.number, 1);

        // Label filter.
        let cli = list(
            repo,
            &IssueFilter {
                label: Some("cli".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(cli.len(), 1);

        // Status filter.
        let todos = list(
            repo,
            &IssueFilter {
                status: Some(IssueStatus::Todo),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(todos.len(), 2);

        // Ready-only: #2 is blocked by #1, so only #1 is ready.
        let ready = list(
            repo,
            &IssueFilter {
                ready_only: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].summary.number, 1);
    }

    #[test]
    fn search_matches_title_and_body_case_insensitively() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        create(
            repo,
            NewIssue {
                body: "Investigate the LOGIN flow".to_string(),
                ..new_issue("Auth bug")
            },
        )
        .unwrap();
        create(repo, new_issue("Unrelated")).unwrap();

        // Matches body text regardless of case.
        let by_body = search(repo, "login", &IssueFilter::default()).unwrap();
        assert_eq!(by_body.len(), 1);
        assert_eq!(by_body[0].summary.number, 1);

        // Matches title.
        let by_title = search(repo, "auth", &IssueFilter::default()).unwrap();
        assert_eq!(by_title.len(), 1);

        // No match.
        assert!(search(repo, "zzzzz", &IssueFilter::default())
            .unwrap()
            .is_empty());

        // An empty query matches every issue.
        let all = search(repo, "", &IssueFilter::default()).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn search_respects_filters_and_readiness() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        create(
            repo,
            NewIssue {
                body: "shared keyword".to_string(),
                priority: IssuePriority::High,
                ..new_issue("one")
            },
        )
        .unwrap();
        create(
            repo,
            NewIssue {
                body: "shared keyword".to_string(),
                dependson: vec![1],
                ..new_issue("two")
            },
        )
        .unwrap();

        let high = search(
            repo,
            "shared",
            &IssueFilter {
                priority: Some(IssuePriority::High),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(high.len(), 1);
        assert_eq!(high[0].summary.number, 1);

        let ready = search(
            repo,
            "shared",
            &IssueFilter {
                ready_only: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].summary.number, 1);
    }
}

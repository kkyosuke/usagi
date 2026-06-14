//! Business logic for task issues: create, read, update, delete, list and
//! search issues stored under `<repo>/.usagi/issues/`.
//!
//! Listing and searching annotate each issue with its dependency *readiness* —
//! whether every issue it `dependson` is already `done` — so callers can surface
//! the tasks that are actually ready to pick up.

use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use chrono::Utc;

use crate::domain::issue::{Issue, IssuePriority, IssueStatus, IssueSummary};
use crate::infrastructure::issue_store::IssueStore;

/// Fields needed to open a new issue. The number and timestamps are assigned by
/// [`create`].
pub struct NewIssue {
    pub title: String,
    pub priority: IssuePriority,
    pub labels: Vec<String>,
    pub dependson: Vec<u32>,
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
            && self.body.is_none()
    }
}

/// Filters applied to listings and searches. An unset field matches everything.
#[derive(Default)]
pub struct IssueFilter {
    pub status: Option<IssueStatus>,
    pub priority: Option<IssuePriority>,
    pub label: Option<String>,
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
    let needle = query.to_lowercase();
    let matched: Vec<IssueSummary> = issues
        .into_iter()
        .filter(|i| {
            i.title.to_lowercase().contains(&needle) || i.body.to_lowercase().contains(&needle)
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

    fn new_issue(title: &str) -> NewIssue {
        NewIssue {
            title: title.to_string(),
            priority: IssuePriority::Medium,
            labels: vec![],
            dependson: vec![],
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

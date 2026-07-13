//! Issue CRUD operations over the issue store.
//!
//! The application-level operations both the human CLI (`usagi issue …`) and the
//! agent-facing MCP tools (`issue_*`) call: create (allocating the next number),
//! fetch, list, update, and delete a task issue. Each takes the injected
//! [`IssueStore`] and, for the mutating operations, the current time (`now`), so
//! this layer stays clock-free and fully testable; the concrete store and clock
//! are bound by the caller.

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::domain::issue::{Issue, IssuePriority, IssueStatus, IssueSummary};
use crate::infrastructure::store::issue::IssueStore;

/// The fields supplied when creating an issue. The number, status, and timestamps
/// are assigned by [`create`], so they are not part of the request.
#[derive(Debug, Clone, Default)]
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

/// A partial update to an existing issue: each `Some` field replaces the stored
/// value, each `None` leaves it unchanged. `parent` / `milestone` nest a second
/// `Option` so a caller can distinguish "leave as is" (`None`) from "clear it"
/// (`Some(None)`).
#[derive(Debug, Clone, Default)]
pub struct IssuePatch {
    pub title: Option<String>,
    pub status: Option<IssueStatus>,
    pub priority: Option<IssuePriority>,
    pub labels: Option<Vec<String>>,
    pub dependson: Option<Vec<u32>>,
    pub related: Option<Vec<u32>>,
    pub parent: Option<Option<u32>>,
    pub milestone: Option<Option<String>>,
    pub body: Option<String>,
}

/// Create a new issue, allocating the next number (one past the highest stored),
/// stamping `now` as its creation and update time, and writing it. Returns the
/// created issue.
///
/// # Errors
///
/// Returns an error when the store cannot allocate a number or write the issue.
#[coverage(off)]
pub fn create(store: &IssueStore, spec: NewIssue, now: DateTime<Utc>) -> Result<Issue> {
    let lock = store.lock()?;
    let number = store.max_number()? + 1;
    let issue = Issue {
        number,
        title: spec.title,
        status: IssueStatus::default(),
        priority: spec.priority,
        labels: spec.labels,
        dependson: spec.dependson,
        related: spec.related,
        parent: spec.parent,
        milestone: spec.milestone,
        created_at: now,
        updated_at: now,
        body: spec.body,
    };
    store.write_locked(&lock, &issue)?;
    Ok(issue)
}

/// Fetch one issue by number, or `None` when it does not exist.
///
/// # Errors
///
/// Returns an error when the backing file cannot be read or parsed.
#[coverage(off)]
pub fn get(store: &IssueStore, number: u32) -> Result<Option<Issue>> {
    store.read(number)
}

/// Metadata summaries for every issue, in number order.
///
/// # Errors
///
/// Returns an error when the index cannot be read and the markdown source cannot
/// be rescanned.
#[coverage(off)]
pub fn list(store: &IssueStore) -> Result<Vec<IssueSummary>> {
    store.summaries()
}

/// Apply `patch` to the issue numbered `number`, stamp `now` as its update time,
/// and write it. Returns the updated issue, or `None` when no such issue exists.
///
/// # Errors
///
/// Returns an error when the issue cannot be read or the write fails.
#[coverage(off)]
pub fn update(
    store: &IssueStore,
    number: u32,
    patch: IssuePatch,
    now: DateTime<Utc>,
) -> Result<Option<Issue>> {
    let lock = store.lock()?;
    let Some(mut issue) = store.read(number)? else {
        return Ok(None);
    };
    if let Some(title) = patch.title {
        issue.title = title;
    }
    if let Some(status) = patch.status {
        issue.status = status;
    }
    if let Some(priority) = patch.priority {
        issue.priority = priority;
    }
    if let Some(labels) = patch.labels {
        issue.labels = labels;
    }
    if let Some(dependson) = patch.dependson {
        issue.dependson = dependson;
    }
    if let Some(related) = patch.related {
        issue.related = related;
    }
    if let Some(parent) = patch.parent {
        issue.parent = parent;
    }
    if let Some(milestone) = patch.milestone {
        issue.milestone = milestone;
    }
    if let Some(body) = patch.body {
        issue.body = body;
    }
    issue.updated_at = now;
    store.write_locked(&lock, &issue)?;
    Ok(Some(issue))
}

/// Delete the issue numbered `number`, returning whether one was removed.
///
/// # Errors
///
/// Returns an error when the lock cannot be taken or a file cannot be removed.
#[coverage(off)]
pub fn delete(store: &IssueStore, number: u32) -> Result<bool> {
    store.remove(number)
}

#[cfg(test)]
mod tests {
    use super::{IssuePatch, NewIssue, create, delete, get, list, update};
    use crate::domain::issue::{IssuePriority, IssueStatus};
    use crate::infrastructure::store::issue::IssueStore;
    use chrono::{DateTime, TimeZone, Utc};

    #[coverage(off)]
    fn ts(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, day, 0, 0, 0).unwrap()
    }

    #[coverage(off)]
    fn store() -> (tempfile::TempDir, IssueStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        (tmp, store)
    }

    #[coverage(off)]
    fn spec(title: &str) -> NewIssue {
        NewIssue {
            title: title.to_string(),
            priority: IssuePriority::High,
            body: "body".to_string(),
            ..Default::default()
        }
    }

    #[test]
    #[coverage(off)]
    fn create_allocates_sequential_numbers_and_defaults_to_todo() {
        let (_tmp, store) = store();
        let first = create(&store, spec("first"), ts(20)).unwrap();
        let second = create(&store, spec("second"), ts(20)).unwrap();
        assert_eq!(first.number, 1);
        assert_eq!(second.number, 2);
        assert_eq!(first.status, IssueStatus::Todo);
        assert_eq!(first.priority, IssuePriority::High);
        assert_eq!(first.created_at, ts(20));
    }

    #[test]
    #[coverage(off)]
    fn get_reads_back_a_created_issue_and_none_for_missing() {
        let (_tmp, store) = store();
        create(&store, spec("first"), ts(20)).unwrap();
        assert_eq!(get(&store, 1).unwrap().unwrap().title, "first");
        assert!(get(&store, 99).unwrap().is_none());
    }

    #[test]
    #[coverage(off)]
    fn list_returns_summaries_in_number_order() {
        let (_tmp, store) = store();
        create(&store, spec("first"), ts(20)).unwrap();
        create(&store, spec("second"), ts(20)).unwrap();
        let numbers: Vec<u32> = list(&store)
            .unwrap()
            .into_iter()
            .map(|s| s.number)
            .collect();
        assert_eq!(numbers, vec![1, 2]);
    }

    #[test]
    #[coverage(off)]
    fn update_applies_only_the_set_fields_and_stamps_the_time() {
        let (_tmp, store) = store();
        create(&store, spec("first"), ts(20)).unwrap();

        let patch = IssuePatch {
            status: Some(IssueStatus::Done),
            milestone: Some(Some("v2".to_string())),
            ..Default::default()
        };
        let updated = update(&store, 1, patch, ts(21)).unwrap().unwrap();
        assert_eq!(updated.status, IssueStatus::Done);
        assert_eq!(updated.milestone.as_deref(), Some("v2"));
        // Untouched fields survive; the time is stamped.
        assert_eq!(updated.title, "first");
        assert_eq!(updated.updated_at, ts(21));
        // Persisted.
        assert_eq!(get(&store, 1).unwrap().unwrap().status, IssueStatus::Done);
    }

    #[test]
    #[coverage(off)]
    fn update_can_replace_every_field_and_clear_the_optionals() {
        let (_tmp, store) = store();
        let mut base = spec("first");
        base.parent = Some(9);
        base.milestone = Some("m".to_string());
        create(&store, base, ts(20)).unwrap();

        let patch = IssuePatch {
            title: Some("renamed".to_string()),
            status: Some(IssueStatus::InProgress),
            priority: Some(IssuePriority::Low),
            labels: Some(vec!["a".to_string()]),
            dependson: Some(vec![2]),
            related: Some(vec![3]),
            parent: Some(None),    // clear
            milestone: Some(None), // clear
            body: Some("new body".to_string()),
        };
        let updated = update(&store, 1, patch, ts(21)).unwrap().unwrap();
        assert_eq!(updated.title, "renamed");
        assert_eq!(updated.priority, IssuePriority::Low);
        assert_eq!(updated.labels, vec!["a".to_string()]);
        assert_eq!(updated.dependson, vec![2]);
        assert_eq!(updated.related, vec![3]);
        assert_eq!(updated.parent, None);
        assert_eq!(updated.milestone, None);
        assert_eq!(updated.body, "new body");
    }

    #[test]
    #[coverage(off)]
    fn update_is_none_for_a_missing_issue() {
        let (_tmp, store) = store();
        assert!(
            update(&store, 1, IssuePatch::default(), ts(21))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    #[coverage(off)]
    fn delete_removes_a_created_issue_and_reports_success() {
        let (_tmp, store) = store();
        create(&store, spec("first"), ts(20)).unwrap();
        assert!(delete(&store, 1).unwrap());
        assert!(get(&store, 1).unwrap().is_none());
        assert!(!delete(&store, 1).unwrap());
    }
}

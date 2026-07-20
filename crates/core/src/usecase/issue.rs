//! Issue CRUD operations over the issue store.
//!
//! The application-level operations both the human CLI (`usagi issue …`) and the
//! agent-facing MCP tools (`issue_*`) call: create (allocating the next number),
//! fetch, list, update, and delete a task issue. Each takes the injected
//! [`IssueStore`] and, for the mutating operations, the current time (`now`), so
//! this layer stays clock-free and fully testable; the concrete store and clock
//! are bound by the caller.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Component, Path};

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::domain::issue::{Issue, IssuePriority, IssueStatus, IssueSummary};
use crate::infrastructure::store::issue::IssueStore;

/// The fields supplied when creating an issue. The number, status, and timestamps
/// are assigned by [`create`], so they are not part of the request.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NewIssue {
    pub title: String,
    #[serde(default)]
    pub priority: IssuePriority,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub dependson: Vec<u32>,
    #[serde(default)]
    pub related: Vec<u32>,
    #[serde(default)]
    pub parent: Option<u32>,
    #[serde(default)]
    pub milestone: Option<String>,
    #[serde(default)]
    pub body: String,
}

/// A partial update to an existing issue: each `Some` field replaces the stored
/// value, each `None` leaves it unchanged. `parent` / `milestone` nest a second
/// `Option` so a caller can distinguish "leave as is" (`None`) from "clear it"
/// (`Some(None)`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct IssuePatch {
    pub title: Option<String>,
    pub status: Option<IssueStatus>,
    pub priority: Option<IssuePriority>,
    pub labels: Option<Vec<String>>,
    pub dependson: Option<Vec<u32>>,
    pub related: Option<Vec<u32>>,
    #[serde(deserialize_with = "double_option")]
    pub parent: Option<Option<u32>>,
    #[serde(deserialize_with = "double_option")]
    pub milestone: Option<Option<String>>,
    pub body: Option<String>,
}

/// Filters applied to issue listings and searches.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct IssueFilter {
    pub status: Option<IssueStatus>,
    pub priority: Option<IssuePriority>,
    pub label: Option<String>,
    pub parent: Option<u32>,
    pub milestone: Option<String>,
    #[serde(rename = "ready")]
    pub ready_only: bool,
}

/// An issue summary annotated with dependency readiness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedIssue {
    pub summary: IssueSummary,
    /// Dependencies that are missing or not yet done.
    pub unmet_deps: Vec<u32>,
}

impl ListedIssue {
    /// Whether the issue can be started now.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.summary.status != IssueStatus::Done && self.unmet_deps.is_empty()
    }
}

#[allow(clippy::option_option)] // Three states encode absent, explicit null, and a value.
fn double_option<'de, T, D>(deserializer: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::deserialize(deserializer)?))
}

impl IssueFilter {
    fn matches(&self, issue: &ListedIssue) -> bool {
        let summary = &issue.summary;
        self.status.is_none_or(|value| summary.status == value)
            && self.priority.is_none_or(|value| summary.priority == value)
            && self
                .label
                .as_ref()
                .is_none_or(|value| summary.labels.iter().any(|label| label == value))
            && self
                .parent
                .is_none_or(|value| summary.parent == Some(value))
            && self
                .milestone
                .as_ref()
                .is_none_or(|value| summary.milestone.as_deref() == Some(value.as_str()))
            && (!self.ready_only || issue.is_ready())
    }
}

/// Refuse git-tracked issue writes from the coordinator checkout.
///
/// A path is writable when it is inside `<repo>/.usagi/sessions/<name>`; this
/// mirrors the repository workflow and pre-commit backstop.
///
/// # Errors
///
/// Returns an error with remediation guidance outside a session worktree.
pub fn ensure_write_allowed(repo_root: &Path) -> Result<()> {
    let names: Vec<_> = repo_root
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();
    if names
        .windows(2)
        .any(|pair| pair[0] == ".usagi" && pair[1] == "sessions")
    {
        return Ok(());
    }
    bail!("issue writes are refused at the workspace root: run them from inside a session worktree")
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
    let number = store.reserve_next_number()?;
    let lock = store.lock()?;
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

/// Search issue titles and bodies, apply metadata filters, and annotate
/// dependency readiness. An empty query lists every issue through the index.
///
/// # Errors
///
/// Returns an error when the store cannot be scanned or indexed.
pub fn search(store: &IssueStore, query: &str, filter: &IssueFilter) -> Result<Vec<ListedIssue>> {
    let all = store.summaries()?;
    let done: HashSet<u32> = all
        .iter()
        .filter(|summary| summary.status == IssueStatus::Done)
        .map(|summary| summary.number)
        .collect();
    let needle = query.to_lowercase();
    let matching: Option<HashSet<u32>> = if needle.is_empty() {
        None
    } else {
        Some(
            store
                .scan_lenient()?
                .into_iter()
                .filter(|issue| {
                    issue.title.to_lowercase().contains(&needle)
                        || issue.body.to_lowercase().contains(&needle)
                })
                .map(|issue| issue.number)
                .collect(),
        )
    };
    Ok(all
        .into_iter()
        .filter(|summary| {
            matching
                .as_ref()
                .is_none_or(|numbers| numbers.contains(&summary.number))
        })
        .map(|summary| ListedIssue {
            unmet_deps: summary
                .dependson
                .iter()
                .copied()
                .filter(|number| !done.contains(number))
                .collect(),
            summary,
        })
        .filter(|issue| filter.matches(issue))
        .collect())
}

/// Render an issue as a self-contained implementation prompt.
#[must_use]
pub fn to_prompt(issue: &Issue) -> String {
    const NONE: &str = "なし";
    let numbers = |values: &[u32]| {
        let mut rendered = String::new();
        for (index, value) in values.iter().enumerate() {
            if index > 0 {
                rendered.push_str(", ");
            }
            let _ = write!(rendered, "{value}");
        }
        if rendered.is_empty() {
            NONE.to_owned()
        } else {
            rendered
        }
    };
    let labels = if issue.labels.is_empty() {
        NONE.to_owned()
    } else {
        issue.labels.join(", ")
    };
    let body = if issue.body.trim().is_empty() {
        "（本文なし）"
    } else {
        issue.body.trim()
    };
    format!(
        "あなたはこのセッションの worktree 内で usagi の issue #{number}「{title}」を実装します。\n\
         \n\
         worktree は既に用意されているため、新しく作成する必要はありません。リポジトリに開発ワークフローや規約のドキュメント（例: `CLAUDE.md` / `.agents/` / `CONTRIBUTING.md` など）があれば、それに従ってください。\n\
         \n\
         ## 進め方\n\
         \n\
         1. 着手時に、この worktree で issue #{number} の status を `in-progress` に更新する（MCP ツール `issue_update`）。これは着手済みを示すこの worktree 内のローカルな進捗表現で、マージするまで基点ブランチには反映されない。\n\
         2. issue の内容を実装し、必要に応じてテストを追加・更新する。\n\
         3. コミット前に、リポジトリの規約に沿ったフォーマット・Lint・テストを実行して通す。\n\
         4. 仕様やドキュメントに影響する変更があれば、対応するドキュメントも更新する。\n\
         5. **PR を開く前に**、この worktree で issue #{number} の status を `done` に更新してコミットする（MCP ツール `issue_update`）。この status 差分は実装差分と同じブランチに載せ、同じ PR に含める（別コミットでよい）。issue の完了を反映できるのはこの worktree（枝）だけなので、PR がマージされて初めて基点ブランチの issue が `done` になる。マージ後に誰も `done` を立て直さないため、必ず PR を開く前にこのコミットを含めること。\n\
         6. PR を作成する。\n\
         \n\
         ## issue #{number}: {title}\n\
         \n\
         - status: {status}\n\
         - priority: {priority}\n\
         - labels: {labels}\n\
         - dependson: {dependson}\n\
         - related: {related}\n\
         - parent: {parent}\n\
         - milestone: {milestone}\n\
         \n\
         {body}\n",
        number = issue.number,
        title = issue.title,
        status = issue.status,
        priority = issue.priority,
        labels = labels,
        dependson = numbers(&issue.dependson),
        related = numbers(&issue.related),
        parent = issue
            .parent
            .map_or_else(|| NONE.to_owned(), |value| value.to_string()),
        milestone = issue.milestone.as_deref().unwrap_or(NONE),
        body = body,
    )
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
    use super::{
        IssueFilter, IssuePatch, NewIssue, create, delete, ensure_write_allowed, get, list, search,
        to_prompt, update,
    };
    use crate::domain::issue::{IssuePriority, IssueStatus};
    use crate::infrastructure::store::issue::IssueStore;
    use chrono::{DateTime, TimeZone, Utc};
    use std::fs;
    use std::sync::{Arc, Barrier};
    use std::thread;

    fn ts(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, day, 0, 0, 0).unwrap()
    }

    fn store() -> (tempfile::TempDir, IssueStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        (tmp, store)
    }

    fn spec(title: &str) -> NewIssue {
        NewIssue {
            title: title.to_string(),
            priority: IssuePriority::High,
            body: "body".to_string(),
            ..Default::default()
        }
    }

    #[test]
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
    fn create_reserves_distinct_numbers_for_concurrent_sibling_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let common_git_dir = tmp.path().join("common.git");
        let first_root = tmp.path().join("first");
        let second_root = tmp.path().join("second");
        for (name, root) in [("first", &first_root), ("second", &second_root)] {
            let private_git_dir = common_git_dir.join("worktrees").join(name);
            fs::create_dir_all(&private_git_dir).unwrap();
            fs::create_dir_all(root).unwrap();
            fs::write(
                root.join(".git"),
                format!("gitdir: {}\n", private_git_dir.display()),
            )
            .unwrap();
            fs::write(private_git_dir.join("commondir"), "../..\n").unwrap();
        }

        let start = Arc::new(Barrier::new(2));
        let create_in = |root: std::path::PathBuf, title: &'static str| {
            let start = Arc::clone(&start);
            thread::spawn(move || {
                let store = IssueStore::new(root);
                start.wait();
                create(&store, spec(title), ts(20)).unwrap().number
            })
        };
        let first = create_in(first_root, "first");
        let second = create_in(second_root, "second");
        let mut numbers = [first.join().unwrap(), second.join().unwrap()];
        numbers.sort_unstable();
        assert_eq!(numbers, [1, 2]);
    }

    #[test]
    fn get_reads_back_a_created_issue_and_none_for_missing() {
        let (_tmp, store) = store();
        create(&store, spec("first"), ts(20)).unwrap();
        assert_eq!(get(&store, 1).unwrap().unwrap().title, "first");
        assert!(get(&store, 99).unwrap().is_none());
    }

    #[test]
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
    fn update_is_none_for_a_missing_issue() {
        let (_tmp, store) = store();
        assert!(
            update(&store, 1, IssuePatch::default(), ts(21))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn delete_removes_a_created_issue_and_reports_success() {
        let (_tmp, store) = store();
        create(&store, spec("first"), ts(20)).unwrap();
        assert!(delete(&store, 1).unwrap());
        assert!(get(&store, 1).unwrap().is_none());
        assert!(!delete(&store, 1).unwrap());
    }

    #[test]
    fn search_matches_text_filters_and_dependency_readiness() {
        let (_tmp, store) = store();
        let done = create(&store, spec("finished"), ts(20)).unwrap();
        update(
            &store,
            done.number,
            IssuePatch {
                status: Some(IssueStatus::Done),
                ..Default::default()
            },
            ts(21),
        )
        .unwrap();
        let mut ready_spec = spec("Alpha task");
        ready_spec.body = "Needle in body".into();
        ready_spec.dependson = vec![done.number];
        ready_spec.labels = vec!["mcp".into()];
        ready_spec.parent = Some(9);
        ready_spec.milestone = Some("v2".into());
        let ready = create(&store, ready_spec, ts(20)).unwrap();
        let mut blocked_spec = spec("blocked");
        blocked_spec.dependson = vec![999];
        create(&store, blocked_spec, ts(20)).unwrap();

        let listed = search(&store, "", &IssueFilter::default()).unwrap();
        assert_eq!(listed.len(), 3);
        assert!(
            listed
                .iter()
                .find(|item| item.summary.number == ready.number)
                .unwrap()
                .is_ready()
        );
        assert_eq!(listed[2].unmet_deps, vec![999]);

        let filtered = search(
            &store,
            "needle",
            &IssueFilter {
                status: Some(IssueStatus::Todo),
                priority: Some(IssuePriority::High),
                label: Some("mcp".into()),
                parent: Some(9),
                milestone: Some("v2".into()),
                ready_only: true,
            },
        )
        .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].summary.number, ready.number);
        assert!(
            search(&store, "absent", &IssueFilter::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn prompt_renders_present_and_empty_metadata() {
        let (_tmp, store) = store();
        let empty = create(
            &store,
            NewIssue {
                title: "empty".into(),
                ..Default::default()
            },
            ts(20),
        )
        .unwrap();
        let empty_prompt = to_prompt(&empty);
        assert!(empty_prompt.contains("labels: なし"));
        assert!(empty_prompt.contains("（本文なし）"));

        let full = create(
            &store,
            NewIssue {
                title: "full".into(),
                labels: vec!["one".into(), "two".into()],
                dependson: vec![1, 2],
                related: vec![3],
                parent: Some(4),
                milestone: Some("m1".into()),
                body: " body ".into(),
                ..Default::default()
            },
            ts(20),
        )
        .unwrap();
        let prompt = to_prompt(&full);
        assert!(prompt.contains("labels: one, two"));
        assert!(prompt.contains("dependson: 1, 2"));
        assert!(prompt.trim_end().ends_with("body"));
    }

    #[test]
    fn write_guard_accepts_sessions_and_refuses_workspace_root() {
        assert!(ensure_write_allowed(std::path::Path::new("/repo/.usagi/sessions/one")).is_ok());
        let error = ensure_write_allowed(std::path::Path::new("/repo")).unwrap_err();
        assert!(error.to_string().contains("workspace root"));
    }

    #[test]
    fn patch_deserialization_distinguishes_absent_and_null_optionals() {
        let absent: IssuePatch = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.parent, None);
        assert_eq!(absent.milestone, None);
        let nulls: IssuePatch =
            serde_json::from_str(r#"{"parent":null,"milestone":null}"#).unwrap();
        assert_eq!(nulls.parent, Some(None));
        assert_eq!(nulls.milestone, Some(None));
    }
}

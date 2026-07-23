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

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::domain::issue::{Issue, IssuePriority, IssueStatus, IssueSummary};
use crate::infrastructure::store::issue::{IssueSourceSnapshot, IssueStore};

#[cfg(test)]
thread_local! {
    static CREATE_RETRY_VALIDATION_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Inject a source change between retry discovery and validation.
#[cfg(test)]
fn set_create_retry_validation_hook(hook: impl FnOnce() + 'static) {
    CREATE_RETRY_VALIDATION_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
fn run_create_retry_validation_hook() {
    CREATE_RETRY_VALIDATION_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

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
    /// More than one source Markdown file claims this issue number.
    pub ambiguous: bool,
    /// Dependencies that are missing or not yet done.
    pub unmet_deps: Vec<u32>,
}

impl ListedIssue {
    /// Whether the issue can be started now.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        !self.ambiguous && self.summary.status != IssueStatus::Done && self.unmet_deps.is_empty()
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
/// Returns an error when a matching retry resolves to an ambiguous number, or
/// when the store cannot allocate a number or write the issue.
pub fn create(store: &IssueStore, spec: NewIssue, now: DateTime<Utc>) -> Result<Issue> {
    let lock = store.lock()?;
    if let Some(matching) = store
        .source_snapshot_locked(&lock)?
        .sources
        .into_iter()
        .find(|source| matches_new_issue_source(&source.issue, &spec))
    {
        let source_number = matching.filename_number.context(format!(
            "matching issue source {} has no numeric filename prefix",
            matching.file
        ))?;
        #[cfg(test)]
        run_create_retry_validation_hook();
        let existing = store.read_locked(source_number)?.context(format!(
            "matching issue source {} disappeared while validating create retry",
            matching.file
        ))?;
        if !matches_new_issue_source(&existing, &spec) {
            bail!(
                "matching issue source {} changed while validating create retry",
                matching.file
            );
        }
        return Ok(existing);
    }
    let number = store.reserve_next_number()?;
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

/// Match the durable identity of an initial create request. Timestamps and the
/// allocated number are deliberately excluded so retrying after an ambiguous
/// response finds the source that already committed instead of allocating a
/// duplicate.
fn matches_new_issue_source(issue: &Issue, spec: &NewIssue) -> bool {
    issue.status == IssueStatus::Todo
        && issue.title == spec.title
        && issue.priority == spec.priority
        && issue.labels == spec.labels
        && issue.dependson == spec.dependson
        && issue.related == spec.related
        && issue.parent == spec.parent
        && issue.milestone == spec.milestone
        && issue.body == spec.body
}

/// Fetch one issue by number, or `None` when it does not exist.
///
/// # Errors
///
/// Returns an error when the backing file cannot be read or parsed, or when
/// multiple source files claim `number`.
pub fn get(store: &IssueStore, number: u32) -> Result<Option<Issue>> {
    store.read(number)
}

/// Metadata summaries for every issue, in number order.
///
/// # Errors
///
/// Returns an error when the index cannot be read and the markdown source cannot
/// be rescanned.
pub fn list(store: &IssueStore) -> Result<Vec<IssueSummary>> {
    store.summaries()
}

/// Search issue titles and bodies, apply metadata filters, and annotate
/// dependency readiness from one lock-protected Markdown source snapshot.
///
/// # Errors
///
/// Returns an error when the store cannot be scanned or indexed.
pub fn search(store: &IssueStore, query: &str, filter: &IssueFilter) -> Result<Vec<ListedIssue>> {
    Ok(search_snapshot(store.source_snapshot()?, query, filter))
}

fn search_snapshot(
    snapshot: IssueSourceSnapshot,
    query: &str,
    filter: &IssueFilter,
) -> Vec<ListedIssue> {
    let mut unsafe_numbers: HashSet<u32> = snapshot
        .claims
        .iter()
        .filter(|(_, files)| files.len() > 1)
        .map(|(number, _)| *number)
        .collect();
    for source in &snapshot.sources {
        if source.filename_number != Some(source.issue.number) {
            unsafe_numbers.insert(source.issue.number);
            if let Some(filename_number) = source.filename_number {
                unsafe_numbers.insert(filename_number);
            }
        }
    }
    let done: HashSet<u32> = snapshot
        .sources
        .iter()
        .filter(|source| {
            source.issue.status == IssueStatus::Done
                && source.filename_number == Some(source.issue.number)
                && !unsafe_numbers.contains(&source.issue.number)
        })
        .map(|source| source.issue.number)
        .collect();
    let needle = query.to_lowercase();
    snapshot
        .sources
        .into_iter()
        .filter(|source| {
            needle.is_empty()
                || source.issue.title.to_lowercase().contains(&needle)
                || source.issue.body.to_lowercase().contains(&needle)
        })
        .map(|source| {
            let ambiguous = unsafe_numbers.contains(&source.issue.number)
                || source
                    .filename_number
                    .is_none_or(|number| unsafe_numbers.contains(&number));
            let summary = source.summary();
            ListedIssue {
                ambiguous,
                unmet_deps: summary
                    .dependson
                    .iter()
                    .copied()
                    .filter(|number| !done.contains(number))
                    .collect(),
                summary,
            }
        })
        .filter(|issue| filter.matches(issue))
        .collect()
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
/// Returns an error when the issue cannot be read unambiguously or the write
/// fails.
pub fn update(
    store: &IssueStore,
    number: u32,
    patch: IssuePatch,
    now: DateTime<Utc>,
) -> Result<Option<Issue>> {
    let lock = store.lock()?;
    let Some(mut issue) = store.read_locked(number)? else {
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
/// Returns an error when the lock cannot be taken, the issue number is
/// ambiguous, or a file cannot be removed.
pub fn delete(store: &IssueStore, number: u32) -> Result<bool> {
    store.remove(number)
}

#[cfg(test)]
mod tests {
    use super::{
        IssueFilter, IssuePatch, NewIssue, create, delete, ensure_write_allowed, get, list, search,
        search_snapshot, set_create_retry_validation_hook, to_prompt, update,
    };
    use crate::domain::frontmatter::FrontmatterDoc;
    use crate::domain::issue::{Issue, IssuePriority, IssueStatus};
    use crate::infrastructure::persistence::json_file::{AtomicWriteStage, fail_next_atomic_write};
    use crate::infrastructure::persistence::store_lock::StoreLock;
    use crate::infrastructure::store::issue::{
        AmbiguousIssueNumber, IssueStore, MismatchedIssueNumber, SourceSnapshotLockPhase,
    };
    use chrono::{DateTime, TimeZone, Utc};
    use fs2::FileExt;
    use std::fs;
    use std::path::Path;
    use std::process::Command;
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

    fn persisted_issue(number: u32, title: &str) -> Issue {
        Issue {
            number,
            title: title.to_string(),
            status: IssueStatus::Todo,
            priority: IssuePriority::Medium,
            labels: vec![],
            dependson: vec![],
            related: vec![],
            parent: None,
            milestone: None,
            created_at: ts(20),
            updated_at: ts(20),
            body: "body".to_string(),
        }
    }

    fn assert_mismatched_crud_has_no_effect(
        store: &IssueStore,
        number: u32,
        mismatch_file: &Path,
        filename_number: Option<u32>,
        source_paths: &[&Path],
    ) {
        let source_before: Vec<_> = source_paths
            .iter()
            .map(|path| fs::read(path).unwrap())
            .collect();
        let index_before = fs::read(store.index_path()).unwrap();
        let dirty = store.dir().join(".derived-dirty");
        let dirty_before = fs::read(&dirty).unwrap();

        let errors = [
            get(store, number).unwrap_err(),
            update(
                store,
                number,
                IssuePatch {
                    status: Some(IssueStatus::Done),
                    ..Default::default()
                },
                ts(21),
            )
            .unwrap_err(),
            delete(store, number).unwrap_err(),
        ];
        for error in errors {
            let mismatch = error.downcast_ref::<MismatchedIssueNumber>().unwrap();
            assert_eq!(mismatch.filename_number, filename_number);
            assert_eq!(mismatch.declared_number, 8);
            assert_eq!(mismatch.file, mismatch_file);
        }

        for (path, before) in source_paths.iter().zip(source_before) {
            assert_eq!(fs::read(path).unwrap(), before);
        }
        assert_eq!(fs::read(store.index_path()).unwrap(), index_before);
        assert_eq!(fs::read(dirty).unwrap(), dirty_before);
    }

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "git {args:?} failed: {stderr}");
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
    fn create_retry_reuses_committed_source_identity_after_derived_failure() {
        let _guard = crate::test_support::process_env_guard();
        let logs = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::infrastructure::paths::DATA_DIR_ENV, logs.path());
        }
        let (tmp, store) = store();
        fail_next_atomic_write(&store.index_path(), AtomicWriteStage::Rename);

        let first = create(&store, spec("same request"), ts(20)).unwrap();
        let authority = tmp.path().join(".usagi/issue-numbers");
        let sequence_before = fs::read(authority.join("sequence.json")).unwrap();
        let markers_before = fs::read_dir(authority.join("reservations"))
            .unwrap()
            .count();
        let retry = create(&store, spec("same request"), ts(21)).unwrap();

        assert_eq!(retry.number, first.number);
        assert_eq!(retry.created_at, first.created_at);
        assert_eq!(store.scan().unwrap().len(), 1);
        assert_eq!(
            fs::read(authority.join("sequence.json")).unwrap(),
            sequence_before
        );
        assert_eq!(
            fs::read_dir(authority.join("reservations"))
                .unwrap()
                .count(),
            markers_before
        );

        unsafe {
            std::env::remove_var(crate::infrastructure::paths::DATA_DIR_ENV);
        }
    }

    #[test]
    fn source_write_failure_consumes_the_reservation_before_retrying() {
        let (tmp, store) = store();
        let failed_source = store.dir().join("001-source-failure.md");
        fail_next_atomic_write(&failed_source, AtomicWriteStage::Rename);

        assert!(create(&store, spec("source failure"), ts(20)).is_err());
        assert!(!failed_source.exists());
        let authority = tmp.path().join(".usagi/issue-numbers");
        assert!(authority.join("reservations/0000000001.reserved").is_file());
        assert!(
            fs::read_to_string(authority.join("sequence.json"))
                .unwrap()
                .contains("\"last_reserved\": 1")
        );

        let retry = create(&store, spec("source failure"), ts(21)).unwrap();
        assert_eq!(retry.number, 2);
        assert!(authority.join("reservations/0000000002.reserved").is_file());
    }

    #[test]
    fn corrupt_allocator_state_prevents_create_without_writing_a_source_or_new_reservation() {
        for corrupt_marker in [false, true] {
            let (tmp, store) = store();
            let authority = tmp.path().join(".usagi/issue-numbers");
            fs::create_dir_all(&authority).unwrap();
            let corrupt_path = if corrupt_marker {
                let reservations = authority.join("reservations");
                fs::create_dir(&reservations).unwrap();
                reservations.join("0000000007.reserved")
            } else {
                authority.join("sequence.json")
            };
            fs::write(&corrupt_path, b"corrupt authority state\n").unwrap();
            let corrupt_before = fs::read(&corrupt_path).unwrap();

            assert!(create(&store, spec("must not be written"), ts(20)).is_err());

            assert_eq!(fs::read(&corrupt_path).unwrap(), corrupt_before);
            assert!(store.scan().unwrap().is_empty());
            assert!(!store.dir().join("001-must-not-be-written.md").exists());
            assert!(!authority.join("legacy-v2-migrated").exists());
            if corrupt_marker {
                assert!(!authority.join("sequence.json").exists());
                assert_eq!(
                    fs::read_dir(authority.join("reservations"))
                        .unwrap()
                        .count(),
                    1
                );
            } else {
                assert!(!authority.join("reservations").exists());
            }
        }
    }

    #[test]
    fn create_retry_rejects_an_ambiguous_matching_number() {
        let (_tmp, store) = store();
        let created = create(&store, spec("same request"), ts(20)).unwrap();
        let first = store.dir().join(created.file_name());
        let second = store.dir().join("001-other.md");
        let mut duplicate = created;
        duplicate.title = "other".to_string();
        fs::write(&second, duplicate.to_markdown()).unwrap();
        let source_before = [fs::read(&first).unwrap(), fs::read(&second).unwrap()];

        let error = create(&store, spec("same request"), ts(21)).unwrap_err();
        let ambiguity = error.downcast_ref::<AmbiguousIssueNumber>().unwrap();
        assert_eq!(ambiguity.number, 1);
        assert_eq!(ambiguity.files, vec![second.clone(), first.clone()]);
        assert_eq!(fs::read(first).unwrap(), source_before[0]);
        assert_eq!(fs::read(second).unwrap(), source_before[1]);
    }

    #[test]
    fn create_retry_checks_the_matching_source_filename_identity() {
        let (_tmp, store) = store();
        let first = create(&store, spec("other"), ts(20)).unwrap();
        let retry = create(&store, spec("same request"), ts(20)).unwrap();
        let first = store.dir().join(first.file_name());
        let moved = store.dir().join("001-retry.md");
        fs::rename(store.dir().join(retry.file_name()), &moved).unwrap();

        let error = create(&store, spec("same request"), ts(21)).unwrap_err();
        let ambiguity = error.downcast_ref::<AmbiguousIssueNumber>().unwrap();
        assert_eq!(ambiguity.number, 1);
        assert_eq!(ambiguity.files, vec![first, moved]);
    }

    #[test]
    fn create_retry_rejects_noncanonical_or_changed_matching_sources() {
        {
            let (_tmp, store) = store();
            let created = create(&store, spec("no prefix"), ts(20)).unwrap();
            fs::rename(
                store.dir().join(created.file_name()),
                store.dir().join("retry.md"),
            )
            .unwrap();
            assert!(
                create(&store, spec("no prefix"), ts(21))
                    .unwrap_err()
                    .to_string()
                    .contains("has no numeric filename prefix")
            );
        }

        {
            let (_tmp, store) = store();
            let created = create(&store, spec("mismatched"), ts(20)).unwrap();
            fs::rename(
                store.dir().join(created.file_name()),
                store.dir().join("002-retry.md"),
            )
            .unwrap();
            assert!(
                create(&store, spec("mismatched"), ts(21))
                    .unwrap_err()
                    .to_string()
                    .contains("declares #1 but its filename claims #2")
            );
        }

        {
            let (_tmp, store) = store();
            let created = create(&store, spec("disappears"), ts(20)).unwrap();
            let source = store.dir().join(created.file_name());
            set_create_retry_validation_hook(move || fs::remove_file(source).unwrap());
            assert!(
                create(&store, spec("disappears"), ts(21))
                    .unwrap_err()
                    .to_string()
                    .contains("disappeared while validating create retry")
            );
        }

        {
            let (_tmp, store) = store();
            let mut created = create(&store, spec("changes"), ts(20)).unwrap();
            let source = store.dir().join(created.file_name());
            created.body = "changed concurrently".to_string();
            let changed = created.to_markdown();
            set_create_retry_validation_hook(move || fs::write(source, changed).unwrap());
            assert!(
                create(&store, spec("changes"), ts(21))
                    .unwrap_err()
                    .to_string()
                    .contains("changed while validating create retry")
            );
        }
    }

    #[test]
    fn to_prompt_contains_workflow_metadata_and_body() {
        let (_tmp, store) = store();
        let issue = create(
            &store,
            NewIssue {
                title: "wire sessions".into(),
                priority: IssuePriority::High,
                labels: vec!["mcp".into()],
                dependson: vec![1, 2],
                parent: Some(9),
                body: "Implement it.".into(),
                ..Default::default()
            },
            ts(20),
        )
        .unwrap();
        let prompt = to_prompt(&issue);
        assert!(prompt.contains("issue #1「wire sessions」"));
        assert!(prompt.contains("labels: mcp"));
        assert!(prompt.contains("dependson: 1, 2"));
        assert!(prompt.contains("parent: 9"));
        assert!(prompt.contains("Implement it."));
        assert!(prompt.contains("issue #1 の status を `done`"));

        let empty = create(&store, NewIssue::default(), ts(21)).unwrap();
        let empty_prompt = to_prompt(&empty);
        assert!(empty_prompt.contains("labels: なし"));
        assert!(empty_prompt.contains("（本文なし）"));
    }

    #[test]
    fn create_reserves_distinct_numbers_for_concurrent_real_git_sibling_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "user.name", "Test"]);
        fs::write(root.join("README.md"), "workspace\n").unwrap();
        git(&root, &["add", "README.md"]);
        git(&root, &["commit", "-q", "-m", "init"]);

        let sessions = root.join(".usagi/sessions");
        fs::create_dir_all(&sessions).unwrap();
        let first_root = sessions.join("first");
        let second_root = sessions.join("second");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "test-first",
                first_root.to_str().unwrap(),
            ],
        );
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "test-second",
                second_root.to_str().unwrap(),
            ],
        );

        let seeded = Issue {
            number: 515,
            title: "unmerged sibling".to_string(),
            status: IssueStatus::Todo,
            priority: IssuePriority::High,
            labels: vec![],
            dependson: vec![],
            related: vec![],
            parent: None,
            milestone: None,
            created_at: ts(19),
            updated_at: ts(19),
            body: "already reserved outside main".to_string(),
        };
        IssueStore::new(&first_root).write(&seeded).unwrap();

        // Production v1 has already reserved the unmerged source number in the
        // Git-common authority. That shared floor is the safe side of the
        // first old-v2 sentinel handshake.
        let authority = root.join(".git/usagi/issue-numbers");
        let reservations = authority.join("reservations");
        fs::create_dir_all(&reservations).unwrap();
        fs::write(
            authority.join("sequence.json"),
            b"{\"version\":1,\"last_reserved\":515}\n",
        )
        .unwrap();
        fs::write(reservations.join("0000000515.reserved"), b"515\n").unwrap();

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
        assert_eq!(numbers, [516, 517]);
        assert!(
            fs::read_to_string(authority.join("sequence.json"))
                .unwrap()
                .contains("\"last_reserved\": 517")
        );
        assert_eq!(
            fs::read_dir(authority.join("reservations"))
                .unwrap()
                .count(),
            3
        );
    }

    #[test]
    fn get_reads_back_a_created_issue_and_none_for_missing() {
        let (_tmp, store) = store();
        create(&store, spec("first"), ts(20)).unwrap();
        assert_eq!(get(&store, 1).unwrap().unwrap().title, "first");
        assert!(get(&store, 99).unwrap().is_none());
    }

    #[test]
    fn missing_store_get_and_search_are_read_only() {
        let (_tmp, store) = store();
        assert!(!store.dir().exists());

        assert!(get(&store, 1).unwrap().is_none());
        assert!(
            search(&store, "", &IssueFilter::default())
                .unwrap()
                .is_empty()
        );

        assert!(!store.dir().exists());
    }

    #[test]
    fn invalid_store_path_is_not_treated_as_an_empty_store() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("not-a-directory");
        fs::write(&root, b"file").unwrap();
        let store = IssueStore::new(root);

        assert!(get(&store, 1).is_err());
        assert!(search(&store, "", &IssueFilter::default()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_store_or_ancestor_is_not_treated_as_an_empty_get_or_search() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".usagi")).unwrap();
        symlink("missing-issues", tmp.path().join(".usagi/issues")).unwrap();
        let store = IssueStore::new(tmp.path());
        assert!(get(&store, 1).is_err());
        assert!(search(&store, "", &IssueFilter::default()).is_err());

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("session");
        fs::create_dir(&root).unwrap();
        symlink("missing-state", root.join(".usagi")).unwrap();
        let store = IssueStore::new(root);
        assert!(get(&store, 1).is_err());
        assert!(search(&store, "", &IssueFilter::default()).is_err());
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
    fn duplicate_number_point_crud_fails_closed_while_lists_expose_every_sibling() {
        let (_tmp, store) = store();
        let created = create(&store, spec("first"), ts(20)).unwrap();
        let first = store.dir().join(created.file_name());
        let second = store.dir().join("001-second.md");
        let mut duplicate = created;
        duplicate.title = "second".to_string();
        duplicate.status = IssueStatus::Done;
        fs::write(&second, duplicate.to_markdown()).unwrap();
        let mut dependent = spec("dependent");
        dependent.dependson = vec![1];
        create(&store, dependent, ts(20)).unwrap();
        let source_before = [fs::read(&first).unwrap(), fs::read(&second).unwrap()];

        let mut listed_files: Vec<_> = list(&store)
            .unwrap()
            .into_iter()
            .map(|summary| summary.file)
            .collect();
        listed_files.sort();
        assert_eq!(
            listed_files,
            ["001-first.md", "001-second.md", "002-dependent.md"]
        );
        let listed = search(&store, "", &IssueFilter::default()).unwrap();
        let duplicate_rows: Vec<_> = listed
            .iter()
            .filter(|issue| issue.summary.number == 1)
            .collect();
        assert_eq!(duplicate_rows.len(), 2);
        assert!(duplicate_rows.iter().all(|issue| issue.ambiguous));
        assert!(duplicate_rows.iter().all(|issue| !issue.is_ready()));
        let dependent = listed
            .iter()
            .find(|issue| issue.summary.number == 2)
            .unwrap();
        assert_eq!(dependent.unmet_deps, vec![1]);
        assert!(!dependent.is_ready());

        let matching = search(&store, "first", &IssueFilter::default()).unwrap();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].summary.file, "001-first.md");

        let errors = [
            get(&store, 1).unwrap_err(),
            update(
                &store,
                1,
                IssuePatch {
                    status: Some(IssueStatus::Done),
                    ..Default::default()
                },
                ts(21),
            )
            .unwrap_err(),
            delete(&store, 1).unwrap_err(),
        ];
        for error in errors {
            let ambiguity = error.downcast_ref::<AmbiguousIssueNumber>().unwrap();
            assert_eq!(ambiguity.number, 1);
            assert_eq!(ambiguity.files, vec![first.clone(), second.clone()]);
        }

        assert_eq!(fs::read(&first).unwrap(), source_before[0]);
        assert_eq!(fs::read(&second).unwrap(), source_before[1]);
    }

    #[test]
    fn filename_and_declared_number_mismatches_fence_get_update_delete_from_both_sides() {
        for with_canonical_eight in [false, true] {
            let (_tmp, store) = store();
            store.write(&persisted_issue(7, "Moved")).unwrap();
            if with_canonical_eight {
                store.write(&persisted_issue(8, "Canonical")).unwrap();
            }
            let moved = store.dir().join("007-moved.md");
            fs::write(&moved, persisted_issue(8, "Moved").to_markdown()).unwrap();
            let canonical = store.dir().join("008-canonical.md");
            let dirty = store.dir().join(".derived-dirty");
            fs::write(&dirty, b"pre-existing rebuild request\n").unwrap();
            let paths: Vec<&Path> = if with_canonical_eight {
                vec![moved.as_path(), canonical.as_path()]
            } else {
                vec![moved.as_path()]
            };

            assert_mismatched_crud_has_no_effect(&store, 7, &moved, Some(7), &paths);
            assert_mismatched_crud_has_no_effect(&store, 8, &moved, Some(7), &paths);
        }

        let (_tmp, store) = store();
        store.write(&persisted_issue(8, "Moved")).unwrap();
        let manual = store.dir().join("manual.md");
        fs::rename(store.dir().join("008-moved.md"), &manual).unwrap();
        let dirty = store.dir().join(".derived-dirty");
        fs::write(&dirty, b"pre-existing rebuild request\n").unwrap();
        assert_mismatched_crud_has_no_effect(&store, 8, &manual, None, &[manual.as_path()]);
    }

    #[test]
    fn duplicate_diagnostics_use_source_paths_and_count_unparseable_siblings() {
        let (_tmp, store) = store();
        let created = create(&store, spec("first"), ts(20)).unwrap();
        update(
            &store,
            created.number,
            IssuePatch {
                status: Some(IssueStatus::Done),
                ..Default::default()
            },
            ts(21),
        )
        .unwrap();
        let first = store.dir().join(created.file_name());
        let copied = store.dir().join("001-copied.md");
        fs::copy(&first, &copied).unwrap();

        let mut copied_rows: Vec<_> = search(&store, "first", &IssueFilter::default())
            .unwrap()
            .into_iter()
            .map(|issue| {
                let ready = issue.is_ready();
                (issue.summary.file, issue.ambiguous, ready)
            })
            .collect();
        copied_rows.sort();
        assert_eq!(
            copied_rows,
            vec![
                ("001-copied.md".to_string(), true, false),
                ("001-first.md".to_string(), true, false),
            ]
        );

        let mut dependent = spec("dependent");
        dependent.dependson = vec![1];
        create(&store, dependent, ts(22)).unwrap();

        let mismatched = fs::read_to_string(&first)
            .unwrap()
            .replacen("number: 1", "number: 2", 1);
        fs::write(&copied, mismatched).unwrap();
        let mismatched_row = search(&store, "first", &IssueFilter::default())
            .unwrap()
            .into_iter()
            .find(|issue| issue.summary.file == "001-copied.md")
            .unwrap();
        assert_eq!(mismatched_row.summary.number, 2);
        assert!(mismatched_row.ambiguous);
        assert!(!mismatched_row.is_ready());

        fs::remove_file(&copied).unwrap();
        fs::write(&copied, "not an issue").unwrap();
        let rows = search(&store, "", &IssueFilter::default()).unwrap();
        assert_eq!(rows.len(), 2);
        let first = rows.iter().find(|row| row.summary.number == 1).unwrap();
        assert_eq!(first.summary.file, "001-first.md");
        assert!(first.ambiguous);
        assert!(!first.is_ready());
        let dependent = rows.iter().find(|row| row.summary.number == 2).unwrap();
        assert_eq!(dependent.unmet_deps, vec![1]);
        assert!(!dependent.is_ready());
    }

    #[test]
    fn search_holds_one_lock_while_capturing_the_source_snapshot() {
        let (_tmp, store) = store();
        let created = create(&store, spec("first"), ts(20)).unwrap();
        let first = store.dir().join(created.file_name());
        let second_source = fs::read(&first).unwrap();
        let lock_path = Arc::new(StoreLock::path(store.dir()));
        let mut observed = Vec::new();
        let snapshot = store
            .source_snapshot_after_lock_for_test(|phase| {
                observed.push(phase);
                let lock_path = Arc::clone(&lock_path);
                let competing_writer_acquired = thread::spawn(move || {
                    let file = fs::File::options()
                        .read(true)
                        .write(true)
                        .open(lock_path.as_ref())
                        .unwrap();
                    file.try_lock_exclusive().is_ok()
                })
                .join()
                .unwrap();
                assert!(
                    !competing_writer_acquired,
                    "cooperative writer acquired the store lock during {phase:?}"
                );
            })
            .unwrap();
        let before = search_snapshot(snapshot, "", &IssueFilter::default());

        assert_eq!(
            observed,
            [
                SourceSnapshotLockPhase::LockAcquired,
                SourceSnapshotLockPhase::DerivedRepaired,
                SourceSnapshotLockPhase::SnapshotCaptured,
            ]
        );
        assert_eq!(before.len(), 1);
        assert!(!before[0].ambiguous);
        assert!(before[0].is_ready());

        {
            let _lock = store.lock().unwrap();
            fs::write(store.dir().join("001-second.md"), second_source).unwrap();
        }
        let after = search(&store, "", &IssueFilter::default()).unwrap();
        assert_eq!(after.len(), 2);
        assert!(after.iter().all(|row| row.ambiguous));
        assert!(after.iter().all(|row| !row.is_ready()));
    }

    #[test]
    fn search_repairs_a_scheduled_derived_rebuild_before_reading_the_source_snapshot() {
        let (_tmp, store) = store();
        create(&store, spec("first"), ts(20)).unwrap();
        fs::remove_file(store.index_path()).unwrap();
        let dirty = store.dir().join(".derived-dirty");
        fs::write(&dirty, b"rebuild from markdown\n").unwrap();

        let rows = search(&store, "", &IssueFilter::default()).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].summary.title, "first");
        assert!(store.index_path().is_file());
        assert!(!dirty.exists());
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

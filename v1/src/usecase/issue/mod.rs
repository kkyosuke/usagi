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
use serde::Deserialize;

use crate::domain::issue::{Issue, IssuePriority, IssueStatus, IssueSummary};
use crate::infrastructure::issue_number_sequence::IssueNumberSequence;
use crate::infrastructure::issue_store::IssueStore;
use crate::usecase::search;
use crate::usecase::session;

mod gantt;
mod render;
mod stats;
mod tree;
mod view;

pub use gantt::gantt;
pub use render::{list_line, readiness_glyph, readiness_marker, stats_line};
pub use stats::{group, GroupBy, IssueStats};
pub use tree::dependency_tree;
pub use view::{IssueView, ListedIssueView};

/// Fields needed to open a new issue. The number and timestamps are assigned by
/// [`create`].
#[derive(Deserialize)]
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

/// A partial update to an existing issue: every `Some` field is applied, every
/// `None` field is left unchanged.
#[derive(Default, Deserialize)]
#[serde(default)]
pub struct IssueChanges {
    pub title: Option<String>,
    pub status: Option<IssueStatus>,
    pub priority: Option<IssuePriority>,
    pub labels: Option<Vec<String>>,
    pub dependson: Option<Vec<u32>>,
    pub related: Option<Vec<u32>>,
    /// Outer `None` leaves the parent unchanged; `Some(None)` clears it;
    /// `Some(Some(n))` sets it.
    #[serde(deserialize_with = "double_option")]
    pub parent: Option<Option<u32>>,
    /// Outer `None` leaves the milestone unchanged; `Some(None)` clears it;
    /// `Some(Some(name))` sets it.
    #[serde(deserialize_with = "double_option")]
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
#[derive(Default, Deserialize)]
#[serde(default)]
pub struct IssueFilter {
    pub status: Option<IssueStatus>,
    pub priority: Option<IssuePriority>,
    pub label: Option<String>,
    pub parent: Option<u32>,
    pub milestone: Option<String>,
    /// Keep only issues that are ready to start (not done, all deps done).
    #[serde(rename = "ready")]
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

/// Deserialize an optional field while preserving the distinction between an
/// absent key (`None`) and an explicit `null` (`Some(None)`). Used to let
/// protocol adapters clear `parent`/`milestone` by passing JSON `null` while
/// still leaving those fields unchanged when the keys are omitted.
fn double_option<'de, T, D>(deserializer: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::deserialize(deserializer)?))
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
    let workspace_root = session::workspace_root(repo_root);
    let sequence = IssueNumberSequence::new(repo_root, &workspace_root);
    // Reserve globally before touching this worktree's store. The reservation
    // is durable even if this process crashes or the later markdown write fails.
    let number = sequence.reserve(|| max_number_in_workspace(&workspace_root))?;
    let now = Utc::now();
    let issue = Issue {
        number,
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

/// The highest issue number currently present anywhere in the workspace.
///
/// Issues live in each worktree's own `.usagi/issues/` (the workspace root and
/// every session under `.usagi/sessions/<name>/`), so a new issue's number is
/// computed across all of them rather than just `repo_root`. Otherwise two
/// sessions branched from the same point would both reuse the next number and
/// collide when their branches merge into `main`.
///
/// Called while the common [`IssueNumberSequence`] lock is held, so the scan and
/// reservation form one process/worktree-global critical section.
fn max_number_in_workspace(workspace_root: &Path) -> Result<u32> {
    let mut max = IssueStore::new(workspace_root).max_number()?;
    for root in session::session_roots(workspace_root) {
        max = max.max(IssueStore::new(&root).max_number()?);
    }
    Ok(max)
}

/// Fetch a single issue by number.
pub fn get(repo_root: &Path, number: u32) -> Result<Option<Issue>> {
    IssueStore::new(repo_root).read(number)
}

/// Render an issue as a self-contained prompt instructing an agent to implement
/// it following the repository's own workflow. The text is fed verbatim to a
/// session's agent (e.g. via the `session_prompt` MCP tool), which already runs
/// inside the session worktree — so the prompt tells the agent not to create
/// another one. The wording stays repository-agnostic (no language- or
/// usagi-specific paths or commands) so it works for any project usagi manages;
/// the only usagi-specific instructions are the `issue_update` status changes,
/// which operate on usagi's own issue store.
pub fn to_prompt(issue: &Issue) -> String {
    use std::fmt::Write as _;
    // Placeholder for an empty metadata field, shared by every field below so the
    // prompt reads consistently and the wording lives in one place.
    const NONE_LABEL: &str = "なし";
    let number = issue.number;
    let title = &issue.title;
    let numbers = |xs: &[u32]| -> String {
        if xs.is_empty() {
            return NONE_LABEL.to_string();
        }
        let mut out = String::new();
        for (i, n) in xs.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let _ = write!(out, "{n}");
        }
        out
    };
    let labels = if issue.labels.is_empty() {
        NONE_LABEL.to_string()
    } else {
        issue.labels.join(", ")
    };
    let parent = issue
        .parent
        .map_or(NONE_LABEL.to_string(), |p| p.to_string());
    let milestone = issue
        .milestone
        .clone()
        .unwrap_or_else(|| NONE_LABEL.to_string());
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
        status = issue.status,
        priority = issue.priority,
        dependson = numbers(&issue.dependson),
        related = numbers(&issue.related),
    )
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
    // An empty query matches every issue, which is exactly what `list` returns —
    // so take its index path instead of reading and parsing every issue's full
    // Markdown body just to match-all and discard the bodies.
    if query.is_empty() {
        return list(repo_root, filter);
    }
    // Tolerant scan: one unparseable file is skipped (and logged) rather than
    // failing the whole query, matching how `list` reads through the index.
    let issues = IssueStore::new(repo_root).scan_lenient()?;
    let done = done_numbers(issues.iter().map(|i| (i.number, i.status)));
    let needle = search::fold_query(query);
    let matched: Vec<IssueSummary> = issues
        .into_iter()
        .filter(|i| search::matches_folded(&needle, &[&i.title, &i.body]))
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
    // Hold the lock across the read and the write so a concurrent `update` (or a
    // `create` that rewrites the index) cannot interleave between the two and
    // clobber this change — the lost update the store lock exists to prevent.
    // Mirrors `create` above.
    let lock = store.lock()?;
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
    store.write_locked(&lock, &issue)?;
    Ok(Some(issue))
}

/// Delete the issue with `number`, returning whether it existed.
pub fn delete(repo_root: &Path, number: u32) -> Result<bool> {
    IssueStore::new(repo_root).remove(number)
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
mod tests;

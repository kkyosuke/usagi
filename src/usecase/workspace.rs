use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::domain::issue::IssueStatus;
use crate::domain::workspace::Workspace;
use crate::domain::workspace_state::PrLink;
use crate::infrastructure::storage::Storage;
use crate::usecase::issue::{self, IssueFilter};
use crate::usecase::workspace_state;

/// Register a new workspace. Fails if the name is already taken.
pub fn add(storage: &Storage, name: &str, path: impl Into<PathBuf>) -> Result<Workspace> {
    // Hold the cross-process lock across the whole read-modify-write so a
    // concurrent writer cannot read the same list and clobber our registration
    // (or both pass the duplicate-name guard for the same name).
    let _lock = storage.lock()?;
    let mut workspaces = storage.load_workspaces()?;
    if workspaces.iter().any(|w| w.name == name) {
        bail!("workspace '{name}' already exists");
    }
    let workspace = Workspace::new(name, path);
    workspaces.push(workspace.clone());
    storage.save_workspaces(&workspaces)?;
    Ok(workspace)
}

/// List all registered workspaces, most recently updated first.
pub fn list(storage: &Storage) -> Result<Vec<Workspace>> {
    let mut workspaces = storage.load_workspaces()?;
    workspaces.sort_by_key(|w| std::cmp::Reverse(w.updated_at));
    Ok(workspaces)
}

/// A registered workspace enriched with the at-a-glance figures the project
/// selection screen shows beside it: how many sessions it has and how many of
/// its issues are still open, and how many pull requests have been discovered
/// across those sessions. The workspace's own `updated_at` carries the last-used
/// time, so it is not duplicated here.
#[derive(Debug, Clone)]
pub struct WorkspaceOverview {
    pub workspace: Workspace,
    /// Sessions recorded under the workspace (`state.json`).
    pub session_count: usize,
    /// Issues not yet `done` in the workspace's issue store.
    pub open_issue_count: usize,
    /// Unique pull requests recorded across the workspace's sessions.
    pub pr_count: usize,
}

/// List every registered workspace (most recently updated first) together with
/// its session and open-issue counts.
///
/// Each count is read from the workspace's own on-disk tree; a workspace whose
/// path is missing or unreadable simply reports zero rather than failing the
/// whole listing, so one broken entry never blanks the screen.
pub fn overviews(storage: &Storage) -> Result<Vec<WorkspaceOverview>> {
    Ok(list(storage)?.into_iter().map(overview_for).collect())
}

/// Build one workspace's overview, counting its sessions and open issues.
///
/// The session count and the unique-PR count both come from the workspace's
/// `state.json`, so it is read **once** here and both are derived from it, rather
/// than loading and parsing it twice (once for the sessions, once for the PRs).
/// A missing or unreadable state yields zero for both, matching the overview's
/// "one broken entry must not blank the screen" policy.
fn overview_for(workspace: Workspace) -> WorkspaceOverview {
    let sessions = workspace_state::recorded_sessions(&workspace.path).unwrap_or_default();
    let session_count = sessions.len();
    let pr_count = PrLink::aggregate(
        sessions
            .into_iter()
            .flat_map(|session| session.worktrees)
            .flat_map(|worktree| worktree.pr),
    )
    .len();
    let open_issue_count = open_issue_count(&workspace.path);
    WorkspaceOverview {
        workspace,
        session_count,
        open_issue_count,
        pr_count,
    }
}

/// Count the issues under `path` that are not yet `done`. Returns zero when the
/// workspace has no issue store (or it cannot be read).
fn open_issue_count(path: &Path) -> usize {
    issue::list(path, &IssueFilter::default())
        .map(|issues| {
            issues
                .iter()
                .filter(|i| i.summary.status != IssueStatus::Done)
                .count()
        })
        .unwrap_or(0)
}

/// Remove a workspace by name. Fails if it does not exist.
pub fn remove(storage: &Storage, name: &str) -> Result<()> {
    let _lock = storage.lock()?;
    let mut workspaces = storage.load_workspaces()?;
    let before = workspaces.len();
    workspaces.retain(|w| w.name != name);
    if workspaces.len() == before {
        bail!("workspace '{name}' not found");
    }
    storage.save_workspaces(&workspaces)
}

/// Update a workspace's last-used time to now.
pub fn touch(storage: &Storage, name: &str) -> Result<Workspace> {
    let _lock = storage.lock()?;
    let mut workspaces = storage.load_workspaces()?;
    let Some(workspace) = workspaces.iter_mut().find(|w| w.name == name) else {
        bail!("workspace '{name}' not found");
    };
    workspace.touch();
    let touched = workspace.clone();
    storage.save_workspaces(&workspaces)?;
    Ok(touched)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_storage() -> (tempfile::TempDir, Storage) {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let storage = Storage::new(dir.path().join("usagi"));
        (dir, storage)
    }

    #[test]
    fn list_defaults_to_empty_when_file_is_missing() {
        let (_dir, storage) = temp_storage();
        assert!(list(&storage).unwrap().is_empty());
    }

    #[test]
    fn add_workspace() {
        let (_dir, storage) = temp_storage();
        let ws = add(&storage, "alpha", "/tmp/alpha").unwrap();
        assert_eq!(ws.name, "alpha");
        assert_eq!(ws.path.to_str().unwrap(), "/tmp/alpha");
        assert_eq!(ws.created_at, ws.updated_at);

        let workspaces = list(&storage).unwrap();
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].name, "alpha");
    }

    #[test]
    fn add_rejects_duplicate_names() {
        let (_dir, storage) = temp_storage();
        add(&storage, "alpha", "/tmp/alpha").unwrap();
        assert!(add(&storage, "alpha", "/tmp/other").is_err());
    }

    #[test]
    fn touch_updates_last_used_time() {
        let (_dir, storage) = temp_storage();
        let added = add(&storage, "alpha", "/tmp/alpha").unwrap();

        let touched = touch(&storage, "alpha").unwrap();
        assert_eq!(touched.name, "alpha");
        assert!(touched.updated_at > added.updated_at);

        let workspaces = list(&storage).unwrap();
        assert_eq!(workspaces[0].updated_at, touched.updated_at);
    }

    #[test]
    fn touch_missing_workspace_errors() {
        let (_dir, storage) = temp_storage();
        assert!(touch(&storage, "ghost").is_err());
    }

    #[test]
    fn list_sorts_most_recently_updated_first() {
        let (_dir, storage) = temp_storage();
        add(&storage, "alpha", "/tmp/alpha").unwrap();
        add(&storage, "beta", "/tmp/beta").unwrap();

        // Touch alpha so it becomes most recently updated
        touch(&storage, "alpha").unwrap();

        let workspaces = list(&storage).unwrap();
        assert_eq!(workspaces.len(), 2);
        assert_eq!(workspaces[0].name, "alpha");
        assert_eq!(workspaces[1].name, "beta");
    }

    #[test]
    fn remove_workspace() {
        let (_dir, storage) = temp_storage();
        add(&storage, "alpha", "/tmp/alpha").unwrap();
        add(&storage, "beta", "/tmp/beta").unwrap();

        remove(&storage, "alpha").unwrap();

        let workspaces = list(&storage).unwrap();
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].name, "beta");
    }

    #[test]
    fn remove_missing_workspace_errors() {
        let (_dir, storage) = temp_storage();
        add(&storage, "alpha", "/tmp/alpha").unwrap();

        assert!(remove(&storage, "beta").is_err());
    }

    #[test]
    fn overviews_count_open_issues_and_sessions() {
        use crate::domain::workspace_state::{
            BranchStatus, PrLink, SessionRecord, WorkspaceState, WorktreeState,
        };
        use crate::infrastructure::workspace_store::WorkspaceStore;
        use crate::usecase::issue::{self, IssueChanges, NewIssue};
        use chrono::Utc;

        let (_dir, storage) = temp_storage();
        // Point the workspace at a real directory so its issue/session stores
        // can be read.
        let repo = tempfile::tempdir().unwrap();
        add(&storage, "alpha", repo.path()).unwrap();

        // Three issues, one of them closed: two remain open.
        for title in ["a", "b", "c"] {
            issue::create(
                repo.path(),
                NewIssue {
                    title: title.to_string(),
                    priority: crate::domain::issue::IssuePriority::Medium,
                    labels: vec![],
                    dependson: vec![],
                    related: vec![],
                    parent: None,
                    milestone: None,
                    body: String::new(),
                },
            )
            .unwrap();
        }
        issue::update(
            repo.path(),
            1,
            IssueChanges {
                status: Some(IssueStatus::Done),
                ..Default::default()
            },
        )
        .unwrap();

        // Two sessions record the same PR URL; the overview counts unique PRs
        // across the workspace, so the duplicate contributes once.
        let pr = PrLink {
            number: 493,
            url: "https://github.com/kkyosuke/usagi/pull/493".to_string(),
        };
        let other_pr = PrLink {
            number: 494,
            url: "https://github.com/kkyosuke/usagi/pull/494".to_string(),
        };
        let now = Utc::now();
        let worktree = |name: &str, prs: Vec<PrLink>| WorktreeState {
            branch: Some(name.to_string()),
            path: repo.path().join(name),
            head: "abcdef0".to_string(),
            primary: false,
            upstream: None,
            status: BranchStatus::New,
            diff: None,
            ahead_behind: None,
            pr: prs,
            updated_at: now,
        };
        let session = |name: &str, prs: Vec<PrLink>| SessionRecord {
            name: name.to_string(),
            display_name: None,
            note: None,
            label_id: None,
            root: repo.path().join(".usagi/sessions").join(name),
            worktrees: vec![worktree(name, prs)],
            created_at: now,
            last_active: None,
        };
        let mut state = WorkspaceState::new();
        state.sessions = vec![
            session("one", vec![pr.clone()]),
            session("two", vec![pr, other_pr]),
        ];
        WorkspaceStore::new(repo.path()).save(&state).unwrap();

        let overviews = overviews(&storage).unwrap();
        assert_eq!(overviews.len(), 1);
        assert_eq!(overviews[0].workspace.name, "alpha");
        // Two sessions were recorded under the workspace state.
        assert_eq!(overviews[0].session_count, 2);
        // Three created, one marked done.
        assert_eq!(overviews[0].open_issue_count, 2);
        // Two unique PR URLs recorded under the workspace.
        assert_eq!(overviews[0].pr_count, 2);
    }

    #[test]
    fn overviews_report_zero_for_a_missing_path() {
        let (_dir, storage) = temp_storage();
        add(&storage, "ghost", "/no/such/path").unwrap();

        let overviews = overviews(&storage).unwrap();
        assert_eq!(overviews.len(), 1);
        assert_eq!(overviews[0].session_count, 0);
        assert_eq!(overviews[0].open_issue_count, 0);
        assert_eq!(overviews[0].pr_count, 0);
    }
}

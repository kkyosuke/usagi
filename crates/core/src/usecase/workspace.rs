//! Workspace operations shared by the CLI and TUI entry paths.
//!
//! [`open`] resolves a path against the global workspace registry. It reuses and
//! touches an existing entry, or registers a new one under a collision-free
//! display name. The registry lock covers the whole read-modify-write so two
//! processes cannot allocate the same name or overwrite each other's updates.
//! A newly seen non-UTF-8 path is returned as a transient workspace rather than
//! rejected: JSON cannot represent it losslessly, so it is deliberately omitted
//! from the registry (and therefore from [`recent`]).
//!
//! [`recent`] enriches every registered workspace with its session, open-issue,
//! and unique-pull-request counts and sorts the result most-recent-first. A
//! broken repository-local store degrades only that workspace's figures to zero;
//! corruption of the global registry itself is still reported to the caller.

use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::domain::issue::IssueStatus;
use crate::domain::pullrequest::PrLink;
use crate::domain::recent::Recent;
use crate::domain::workspace::{Workspace, WorkspaceOverview};
use crate::infrastructure::store::issue::IssueStore;
use crate::infrastructure::store::state::WorkspaceStateStore;
use crate::infrastructure::store::workspace::Storage;

/// Resolve `path` to a registered workspace, registering it when necessary, and
/// stamp its `updated_at` with `now`.
///
/// Path identity is exact [`Path`] equality. The path stays a path throughout
/// the operation (it is never converted to a UTF-8 string); only its final
/// component is converted lossily when deriving the human-facing display name.
/// An existing entry with the same path is reused even if its display name no
/// longer matches the directory name. A new entry uses the final component, or
/// `"workspace"` when none exists, with `-2`, `-3`, and so on appended when a
/// different path already owns that name.
///
/// The registry's JSON representation cannot encode a non-UTF-8 path. When a
/// newly seen `path` is not UTF-8, this returns a transient [`Workspace`] so the
/// caller can still open it, but does not add it to the registry. Consequently
/// that transient workspace does not appear in [`recent`].
///
/// The registry lock is held across load, resolution/touch, and save.
///
/// # Errors
///
/// Returns an error when the registry lock cannot be acquired or the registry
/// cannot be read or written.
pub fn open(storage: &Storage, path: &Path, now: DateTime<Utc>) -> Result<Workspace> {
    let _lock = storage.lock()?;
    let mut workspaces = storage.load_workspaces()?;
    let (workspace, persist) = resolve_or_register(&mut workspaces, path, now);
    if persist {
        storage.save_workspaces(&workspaces)?;
    }
    Ok(workspace)
}

/// Remove the registered workspaces whose paths are in `paths`.
///
/// This is intentionally a registry operation owned by core. Callers decide
/// which entries are stale (for example after checking the filesystem), while
/// this usecase keeps the read-modify-write transaction under the registry
/// lock. The returned values are the entries that were actually removed.
///
/// # Errors
///
/// Returns an error when the registry lock cannot be acquired or the registry
/// cannot be read or written.
pub fn remove(storage: &Storage, paths: &[std::path::PathBuf]) -> Result<Vec<Workspace>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let _lock = storage.lock()?;
    let mut workspaces = storage.load_workspaces()?;
    let mut removed = Vec::new();
    workspaces.retain(|workspace| {
        let should_remove = paths.iter().any(|path| path == &workspace.path);
        if should_remove {
            removed.push(workspace.clone());
        }
        !should_remove
    });
    if !removed.is_empty() {
        storage.save_workspaces(&workspaces)?;
    }
    Ok(removed)
}

/// Build recent-list entries for all registered workspaces, ordered by
/// `updated_at` descending.
///
/// Only single-workspace [`Recent::Workspace`] entries are produced. Unite
/// recents have their own persistence lifecycle and are not synthesized from
/// the registry. Missing or unreadable repository-local state and issue stores
/// contribute zero counts without hiding healthy sibling workspaces.
///
/// # Errors
///
/// Returns an error when the global workspace registry cannot be read. Errors
/// confined to an individual registered workspace are degraded to zero counts.
pub fn recent(storage: &Storage) -> Result<Vec<Recent>> {
    let mut workspaces = storage.load_workspaces()?;
    workspaces.sort_by_key(|workspace| std::cmp::Reverse(workspace.updated_at));
    Ok(workspaces
        .into_iter()
        .map(overview_for)
        .map(Recent::Workspace)
        .collect())
}

fn resolve_or_register(
    workspaces: &mut Vec<Workspace>,
    path: &Path,
    now: DateTime<Utc>,
) -> (Workspace, bool) {
    if let Some(workspace) = workspaces
        .iter_mut()
        .find(|workspace| workspace.path == path)
    {
        workspace.updated_at = now;
        return (workspace.clone(), true);
    }

    let base_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "workspace".to_string());
    let name = available_name(workspaces, &base_name);
    let workspace = Workspace {
        name,
        path: path.to_path_buf(),
        created_at: now,
        updated_at: now,
    };
    let persist = path.to_str().is_some();
    if persist {
        workspaces.push(workspace.clone());
    }
    (workspace, persist)
}

fn available_name(workspaces: &[Workspace], base: &str) -> String {
    let is_taken = |candidate: &str| {
        workspaces
            .iter()
            .any(|workspace| workspace.name == candidate)
    };
    if !is_taken(base) {
        return base.to_string();
    }

    let mut suffix = 2_u64;
    loop {
        let candidate = format!("{base}-{suffix}");
        if !is_taken(&candidate) {
            return candidate;
        }
        suffix += 1;
    }
}

fn overview_for(workspace: Workspace) -> WorkspaceOverview {
    let state = WorkspaceStateStore::new(&workspace.path)
        .load()
        .ok()
        .flatten();
    let session_count = state.as_ref().map_or(0, |state| state.sessions.len());
    let pr_count = state.as_ref().map_or(0, |state| {
        state
            .sessions
            .iter()
            .flat_map(|session| &session.prs)
            .map(PrLink::pr_key)
            .collect::<HashSet<_>>()
            .len()
    });
    let open_issue_count = IssueStore::new(&workspace.path)
        .summaries()
        .map_or(0, |issues| {
            issues
                .iter()
                .filter(|issue| issue.status != IssueStatus::Done)
                .count()
        });

    WorkspaceOverview::new(workspace, session_count, open_issue_count, pr_count)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use chrono::{DateTime, TimeZone, Utc};

    use super::{open, recent, remove};
    use crate::domain::issue::{Issue, IssuePriority, IssueStatus};
    use crate::domain::note::Scratchpad;
    use crate::domain::pullrequest::PrLink;
    use crate::domain::recent::Recent;
    use crate::domain::session::{SessionOrigin, SessionRecord};
    use crate::domain::workspace::{Workspace, WorkspaceOverview};
    use crate::domain::workspace_state::WorkspaceState;
    use crate::infrastructure::store::issue::IssueStore;
    use crate::infrastructure::store::state::WorkspaceStateStore;
    use crate::infrastructure::store::workspace::Storage;

    fn ts(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, day, 0, 0, 0).unwrap()
    }

    fn workspace(name: &str, path: impl Into<PathBuf>, updated_at: DateTime<Utc>) -> Workspace {
        Workspace {
            name: name.to_string(),
            path: path.into(),
            created_at: ts(1),
            updated_at,
        }
    }

    fn storage() -> (tempfile::TempDir, Storage) {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("home"));
        (tmp, storage)
    }

    fn session(name: &str, root: &Path, prs: Vec<PrLink>) -> SessionRecord {
        SessionRecord {
            name: name.to_string(),
            display_name: None,
            origin: SessionOrigin::Human,
            started_from: None,
            root: root.join(".usagi/sessions").join(name),
            created_at: ts(2),
            last_active: None,
            notes: Scratchpad::default(),
            prs,
        }
    }

    fn issue(number: u32, status: IssueStatus) -> Issue {
        Issue {
            number,
            title: format!("issue {number}"),
            status,
            priority: IssuePriority::Medium,
            labels: Vec::new(),
            dependson: Vec::new(),
            related: Vec::new(),
            parent: None,
            milestone: None,
            created_at: ts(2),
            updated_at: ts(2),
            body: String::new(),
        }
    }

    #[test]
    fn open_registers_a_new_path_with_injected_timestamps() {
        let (tmp, storage) = storage();
        let path = tmp.path().join("alpha");

        let opened = open(&storage, &path, ts(10)).unwrap();

        assert_eq!(opened.name, "alpha");
        assert_eq!(opened.path, path);
        assert_eq!(opened.created_at, ts(10));
        assert_eq!(opened.updated_at, ts(10));
        assert_eq!(storage.load_workspaces().unwrap(), vec![opened]);
    }

    #[test]
    fn open_reuses_the_same_path_and_touches_it_without_duplicating() {
        let (tmp, storage) = storage();
        let path = tmp.path().join("alpha");
        let registered = workspace("custom-label", &path, ts(3));
        storage
            .save_workspaces(std::slice::from_ref(&registered))
            .unwrap();

        let opened = open(&storage, &path, ts(11)).unwrap();

        assert_eq!(opened.name, "custom-label");
        assert_eq!(opened.created_at, registered.created_at);
        assert_eq!(opened.updated_at, ts(11));
        assert_eq!(storage.load_workspaces().unwrap(), vec![opened]);
    }

    #[test]
    fn open_avoids_names_owned_by_different_paths() {
        let (tmp, storage) = storage();
        let first = workspace("project", "/first/project", ts(3));
        let second = workspace("project-2", "/second/project", ts(4));
        storage.save_workspaces(&[first, second]).unwrap();
        let third_path = tmp.path().join("third/project");

        let opened = open(&storage, &third_path, ts(12)).unwrap();

        assert_eq!(opened.name, "project-3");
        assert_eq!(storage.load_workspaces().unwrap().len(), 3);
    }

    #[test]
    fn open_uses_a_fallback_name_when_the_path_has_no_final_component() {
        let (_tmp, storage) = storage();

        let opened = open(&storage, Path::new("/"), ts(12)).unwrap();

        assert_eq!(opened.name, "workspace");
        assert_eq!(opened.path, Path::new("/"));
    }

    #[test]
    fn open_reports_an_unreadable_registry() {
        let (tmp, storage) = storage();
        fs::create_dir_all(storage.dir()).unwrap();
        fs::write(storage.dir().join("workspaces.json"), "{ broken").unwrap();

        assert!(open(&storage, tmp.path(), ts(12)).is_err());
    }

    #[test]
    fn remove_deletes_only_the_requested_registered_paths() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::new(dir.path());
        let alpha = workspace("alpha", "/tmp/alpha", ts(1));
        let beta = workspace("beta", "/tmp/beta", ts(2));
        storage
            .save_workspaces(&[alpha.clone(), beta.clone()])
            .unwrap();

        assert_eq!(
            remove(&storage, std::slice::from_ref(&alpha.path)).unwrap(),
            vec![alpha]
        );
        assert_eq!(storage.load_workspaces().unwrap(), vec![beta]);
        assert!(
            remove(&storage, &[PathBuf::from("/tmp/unknown")])
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn open_keeps_a_new_non_utf8_path_transient_and_intact() {
        use std::ffi::OsString;
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let (_tmp, storage) = storage();
        let path = PathBuf::from(OsString::from_vec(b"/tmp/project-\xff".to_vec()));

        let opened = open(&storage, &path, ts(12)).unwrap();

        assert_eq!(
            opened.path.as_os_str().as_bytes(),
            path.as_os_str().as_bytes()
        );
        assert!(storage.load_workspaces().unwrap().is_empty());
        assert!(recent(&storage).unwrap().is_empty());
    }

    #[test]
    fn recent_is_empty_for_an_empty_registry() {
        let (_tmp, storage) = storage();
        assert!(recent(&storage).unwrap().is_empty());
    }

    #[test]
    fn recent_sorts_and_counts_sessions_open_issues_and_unique_prs() {
        let (tmp, storage) = storage();
        let alpha_root = tmp.path().join("alpha");
        let beta_root = tmp.path().join("beta");
        storage
            .save_workspaces(&[
                workspace("alpha", &alpha_root, ts(5)),
                workspace("beta", &beta_root, ts(9)),
            ])
            .unwrap();

        let shared = PrLink::new(7, "https://example.test/org/repo/pull/7");
        let shared_files = PrLink::new(7, "https://example.test/org/repo/pull/7/files");
        let other = PrLink::new(8, "https://example.test/org/repo/pull/8");
        WorkspaceStateStore::new(&beta_root)
            .save(&WorkspaceState {
                sessions: vec![
                    session("one", &beta_root, vec![shared, other]),
                    session("two", &beta_root, vec![shared_files]),
                ],
                root_notes: Scratchpad::default(),
                updated_at: ts(9),
            })
            .unwrap();
        let issue_store = IssueStore::new(&beta_root);
        issue_store.write(&issue(1, IssueStatus::Todo)).unwrap();
        issue_store
            .write(&issue(2, IssueStatus::InProgress))
            .unwrap();
        issue_store.write(&issue(3, IssueStatus::Done)).unwrap();

        let items = recent(&storage).unwrap();

        assert_eq!(
            items,
            vec![
                Recent::Workspace(WorkspaceOverview::new(
                    workspace("beta", &beta_root, ts(9)),
                    2,
                    2,
                    2,
                )),
                Recent::Workspace(WorkspaceOverview::new(
                    workspace("alpha", &alpha_root, ts(5)),
                    0,
                    0,
                    0,
                )),
            ]
        );
    }

    #[test]
    fn recent_degrades_a_broken_workspace_without_hiding_siblings() {
        let (tmp, storage) = storage();
        let healthy_root = tmp.path().join("healthy");
        let broken_root = tmp.path().join("broken");
        storage
            .save_workspaces(&[
                workspace("healthy", &healthy_root, ts(5)),
                workspace("broken", &broken_root, ts(9)),
            ])
            .unwrap();
        fs::create_dir_all(broken_root.join(".usagi")).unwrap();
        fs::write(broken_root.join(".usagi/state.json"), "{ broken").unwrap();
        fs::write(broken_root.join(".usagi/issues"), "not a directory").unwrap();

        let items = recent(&storage).unwrap();

        assert_eq!(
            items,
            vec![
                Recent::Workspace(WorkspaceOverview::new(
                    workspace("broken", &broken_root, ts(9)),
                    0,
                    0,
                    0,
                )),
                Recent::Workspace(WorkspaceOverview::new(
                    workspace("healthy", &healthy_root, ts(5)),
                    0,
                    0,
                    0,
                )),
            ]
        );
    }

    #[test]
    fn recent_reports_a_broken_global_registry() {
        let (_tmp, storage) = storage();
        fs::create_dir_all(storage.dir()).unwrap();
        fs::write(storage.dir().join("workspaces.json"), "{ broken").unwrap();

        assert!(recent(&storage).is_err());
    }
}

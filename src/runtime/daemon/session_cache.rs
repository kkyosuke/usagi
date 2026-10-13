//! Filesystem and runtime-owner adapters for session-cache retention.

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use usagi_core::domain::id::{SessionId, TerminalRef};
use usagi_core::infrastructure::git::GitRunner as _;
use usagi_core::infrastructure::runtime_model::WorkspaceSessionConfig;
use usagi_core::infrastructure::session_snapshot::SessionListSnapshot;
use usagi_daemon::usecase::session_cache::{
    self, CacheCandidate, CacheNode, CacheTreeIo, CacheUsage, SessionCacheIo,
};
use usagi_daemon::usecase::terminal::TerminalRuntimeState;

use super::{ConnectionWorkspace, SharedAgentRuntime, SharedTerminalRuntime, SystemGit};

type Activity = Vec<(TerminalRef, TerminalRuntimeState)>;

fn generations_quiet(roles: &[usagi_daemon::usecase::generation::GenerationRole]) -> bool {
    use usagi_daemon::usecase::generation::GenerationRole;
    let mut active = 0;
    for role in roles {
        match role {
            GenerationRole::Active => active += 1,
            GenerationRole::Retired => {}
            GenerationRole::Standby | GenerationRole::Draining => return false,
        }
    }
    active == 1
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=session_cache_owner_lock_refuses_poisoned_state
fn lock_owner<T>(owner: &std::sync::Mutex<T>) -> io::Result<std::sync::MutexGuard<'_, T>> {
    match owner.lock() {
        Ok(guard) => Ok(guard),
        Err(_) => Err(io::Error::other("cache cleanup owner is unavailable")),
    }
}

#[allow(clippy::needless_pass_by_value)] // Result::map_err passes its error to this callback by value.
fn session_error(error: usagi_daemon::usecase::session_runtime::SessionRuntimeError) -> io::Error {
    io::Error::other(error.safe_message())
}

pub(super) struct SystemCacheTree;

impl CacheTreeIo for SystemCacheTree {
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=session_cache_fs_keeps_tracked_and_symlinked_directories
    fn node(&self, path: &Path) -> io::Result<CacheNode> {
        let metadata = fs::symlink_metadata(path)?;
        Ok(CacheNode {
            directory: metadata.is_dir(),
            symlink: metadata.is_symlink(),
            usage: CacheUsage {
                bytes: metadata.blocks().saturating_mul(512),
                modified: metadata
                    .modified()?
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                identity: (metadata.dev(), metadata.ino()),
            },
        })
    }

    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=session_cache_fs_keeps_tracked_and_symlinked_directories
    fn children(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        let mut paths = Vec::new();
        for entry in fs::read_dir(path)? {
            paths.push(entry?.path());
        }
        Ok(paths)
    }
}

/// Each ancestor must be a real directory; canonicalization alone would allow
/// following a symlink to another directory inside or outside the worktree.
fn confined_directory(io: &impl CacheTreeIo, root: &Path, relative: &Path) -> io::Result<PathBuf> {
    let mut path = root.to_path_buf();
    for component in relative.components() {
        path.push(component);
        let node = io.node(&path)?;
        if !node.directory || node.symlink {
            return Err(io::Error::other(
                "cache path contains a symlink or non-directory",
            ));
        }
    }
    Ok(path)
}

fn protected(activity: &Activity, session: SessionId) -> bool {
    activity.iter().any(|(terminal, state)| {
        (terminal.session_id.is_none() || terminal.session_id == Some(session))
            && session_cache::runtime_blocks_cleanup(*state)
    })
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=session_cache_fs_keeps_tracked_and_symlinked_directories
fn has_tracked_files(root: &Path, relative: &Path) -> io::Result<bool> {
    let Some(path) = relative.to_str() else {
        return Err(io::Error::other("invalid cache path"));
    };
    let result = SystemGit
        .run(root, &["--literal-pathspecs", "ls-files", "-z", "--", path])
        .map_err(io::Error::other)?;
    if !result.success {
        return Err(io::Error::other("could not verify cache files against Git"));
    }
    Ok(!result.stdout.is_empty())
}

struct SystemSessionCaches<'a> {
    bound: &'a ConnectionWorkspace,
    agent: &'a SharedAgentRuntime,
    terminal: &'a SharedTerminalRuntime,
    activity: (Activity, Activity),
    executable: PathBuf,
}

impl SessionCacheIo for SystemSessionCaches<'_> {
    #[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=session_cache_cleanup_reports_usage_and_keeps_the_session
    fn candidates(&mut self, paths: &[PathBuf]) -> io::Result<Vec<CacheCandidate>> {
        // Match the launch lock order: Agent -> terminal -> session scope.
        let agent = lock_owner(&self.agent.owner)?;
        let terminal = lock_owner(self.terminal)?;
        let runtime = lock_owner(self.bound.sessions())?;
        let snapshot: SessionListSnapshot =
            serde_json::from_value(runtime.snapshot().map_err(session_error)?)?;
        let workspace = self.bound.tenant.workspace_id();
        self.activity = (
            agent.cache_cleanup_activity(workspace),
            terminal.cache_cleanup_activity(workspace),
        );
        let mut scopes = Vec::new();
        for item in snapshot.sessions {
            if let Ok(scope) = runtime.session_scope_by_id(item.session.session_id) {
                scopes.push((scope, item.session.name));
            }
        }
        drop(runtime);
        drop(terminal);
        drop(agent);
        let mut candidates = Vec::new();
        for (scope, name) in scopes {
            for relative in paths {
                let Ok(root) =
                    session_directory(&SystemCacheTree, self.bound.tenant.root(), &scope.path)
                else {
                    continue;
                };
                let Ok(path) = confined_directory(&SystemCacheTree, &root, relative) else {
                    continue;
                };
                let tracked = has_tracked_files(&scope.path, relative)?;
                let Ok(usage) = session_cache::measure(&SystemCacheTree, &path) else {
                    continue;
                };
                candidates.push(CacheCandidate {
                    session: scope.session_id,
                    session_name: name.clone(),
                    path: relative.clone(),
                    usage,
                    protected: tracked
                        || self.executable.starts_with(&path)
                        || protected(&self.activity.0, scope.session_id)
                        || protected(&self.activity.1, scope.session_id),
                });
            }
        }
        Ok(candidates)
    }

    #[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=session_cache_cleanup_reports_usage_and_keeps_the_session
    fn detach(&mut self, candidate: &CacheCandidate) -> io::Result<Option<PathBuf>> {
        let agent = lock_owner(&self.agent.owner)?;
        let terminal = lock_owner(self.terminal)?;
        let workspace = self.bound.tenant.workspace_id();
        let activity = (
            agent.cache_cleanup_activity(workspace),
            terminal.cache_cleanup_activity(workspace),
        );
        // A process may have started and exited during the scan. Retain its
        // newly used cache even if it has already stopped at revalidation.
        if activity != self.activity
            || protected(&activity.0, candidate.session)
            || protected(&activity.1, candidate.session)
        {
            return Ok(None);
        }
        let runtime = lock_owner(self.bound.sessions())?;
        let scope = runtime
            .session_scope_by_id(candidate.session)
            .map_err(session_error)?;
        let root = session_directory(&SystemCacheTree, self.bound.tenant.root(), &scope.path)?;
        if self.executable.starts_with(root.join(&candidate.path)) {
            return Ok(None);
        }
        detach_directory(&root, &candidate.path, candidate.usage.identity)
    }

    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=session_cache_fs_keeps_tracked_and_symlinked_directories
    fn remove(&mut self, detached: &Path) -> io::Result<()> {
        fs::remove_dir_all(detached)
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=session_cache_fs_keeps_tracked_and_symlinked_directories
fn detach_directory(
    root: &Path,
    relative: &Path,
    identity: (u64, u64),
) -> io::Result<Option<PathBuf>> {
    let path = confined_directory(&SystemCacheTree, root, relative)?;
    if SystemCacheTree.node(&path)?.usage.identity != identity || has_tracked_files(root, relative)?
    {
        return Ok(None);
    }
    // Keep the detached tree in the same filesystem. Renaming is short; slow
    // removal happens after the runtime locks are released.
    let state = root.join(".usagi");
    match fs::create_dir(&state) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    confined_directory(&SystemCacheTree, root, Path::new(".usagi"))?;
    let trash = state.join("cache-trash");
    match fs::create_dir(&trash) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    confined_directory(&SystemCacheTree, &state, Path::new("cache-trash"))?;
    let destination = trash.join(usagi_core::domain::id::OperationId::new().to_string());
    fs::rename(path, &destination)?;
    Ok(Some(destination))
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=session_cache_cleanup_reports_usage_and_keeps_the_session
pub(super) fn clean_session_caches(
    bound: &ConnectionWorkspace,
    agent: &SharedAgentRuntime,
    terminal: &SharedTerminalRuntime,
    apply: bool,
) -> io::Result<serde_json::Value> {
    let policy = WorkspaceSessionConfig::load(bound.tenant.root())
        .map_err(io::Error::other)?
        .cache_cleanup()
        .clone();
    let mut io = SystemSessionCaches {
        bound,
        agent,
        terminal,
        activity: (Vec::new(), Vec::new()),
        executable: std::env::current_exe()?,
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if apply
        && !policy.paths.is_empty()
        && super::trusted_generations()
            .map_err(io::Error::other)?
            .len()
            != 1
    {
        return Err(io::Error::other(
            "cache cleanup waits until daemon handoff finishes",
        ));
    }
    if apply && !policy.paths.is_empty() {
        let data = usagi_core::infrastructure::paths::data_dir().map_err(io::Error::other)?;
        let Some(document) =
            usagi_daemon::infrastructure::generation_registry::read_registry_document(&data)
                .map_err(io::Error::other)?
        else {
            return Err(io::Error::other(
                "cache cleanup generation ownership is unavailable",
            ));
        };
        let mut roles = Vec::new();
        for entry in document.generations {
            roles.push(entry.role);
        }
        if !generations_quiet(&roles) {
            return Err(io::Error::other(
                "cache cleanup waits until daemon handoff finishes",
            ));
        }
    }
    if apply {
        purge_detached_caches(bound)?;
        let report = session_cache::clean(&mut io, &policy, now)?;
        Ok(
            serde_json::json!({"mode": "apply", "removed": report.removed, "removed_bytes": report.removed_bytes, "deferred": report.deferred}),
        )
    } else {
        let candidates = io.candidates(&policy.paths)?;
        let plan = session_cache::plan(&policy, &candidates, now);
        let mut rows = Vec::new();
        for (index, candidate) in candidates.iter().enumerate() {
            rows.push(serde_json::json!({"session":candidate.session_name, "path":candidate.path, "bytes":candidate.usage.bytes, "protected":candidate.protected, "selected":plan.contains(&index)}));
        }
        Ok(
            serde_json::json!({"mode":"dry_run", "candidates":rows, "enabled":!policy.paths.is_empty(), "total_bytes":session_cache::total_bytes(&candidates)}),
        )
    }
}

fn session_directory(
    io: &impl CacheTreeIo,
    workspace: &Path,
    session: &Path,
) -> io::Result<PathBuf> {
    if session.parent() != Some(workspace.join(".usagi/sessions").as_path()) {
        return Err(io::Error::other(
            "cache session is outside the managed namespace",
        ));
    }
    let relative = session.strip_prefix(workspace).map_err(io::Error::other)?;
    confined_directory(io, workspace, relative)
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=session_cache_cleanup_reports_usage_and_keeps_the_session
fn purge_detached_caches(bound: &ConnectionWorkspace) -> io::Result<()> {
    let runtime = lock_owner(bound.sessions())?;
    let snapshot: SessionListSnapshot =
        serde_json::from_value(runtime.snapshot().map_err(session_error)?)?;
    let mut roots = Vec::new();
    for item in snapshot.sessions {
        if let Ok(scope) = runtime.session_scope_by_id(item.session.session_id) {
            roots.push(scope.path);
        }
    }
    drop(runtime);
    for root in roots {
        if session_directory(&SystemCacheTree, bound.tenant.root(), &root).is_err() {
            continue;
        }
        let Ok(trash) =
            confined_directory(&SystemCacheTree, &root, Path::new(".usagi/cache-trash"))
        else {
            continue;
        };
        for entry in fs::read_dir(&trash)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if usagi_core::domain::id::OperationId::parse(name).is_err() {
                continue;
            }
            if !has_tracked_files(&root, &Path::new(".usagi/cache-trash").join(name))? {
                fs::remove_dir_all(entry.path())?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::process::Command;
    use usagi_core::domain::id::{DaemonGeneration, TerminalId, WorkspaceId, WorktreeId};

    use super::*;

    #[test]
    fn session_cache_owner_lock_refuses_poisoned_state() {
        let owner = std::sync::Mutex::new(());
        assert!(lock_owner(&owner).is_ok());
        let _ = std::panic::catch_unwind(|| {
            let _guard = owner.lock().unwrap();
            panic!("poison fixture");
        });
        assert!(lock_owner(&owner).is_err());
    }

    #[test]
    fn session_cache_waits_for_standby_and_draining_generations() {
        use usagi_daemon::usecase::generation::GenerationRole::{
            Active, Draining, Retired, Standby,
        };
        assert!(generations_quiet(&[Active]));
        assert!(generations_quiet(&[Active, Retired]));
        for roles in [
            vec![],
            vec![Standby],
            vec![Active, Active],
            vec![Active, Standby],
            vec![Active, Draining],
        ] {
            assert!(!generations_quiet(&roles));
        }
    }

    #[test]
    fn session_cache_fs_keeps_tracked_and_symlinked_directories() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
        fs::create_dir(root.join("target")).unwrap();
        fs::write(root.join("target/source"), "retained").unwrap();
        assert!(
            Command::new("git")
                .args(["add", "target/source"])
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
        let identity = SystemCacheTree
            .node(&root.join("target"))
            .unwrap()
            .usage
            .identity;
        assert!(has_tracked_files(root, Path::new("target")).unwrap());
        assert_eq!(
            detach_directory(root, Path::new("target"), identity).unwrap(),
            None
        );
        assert!(
            Command::new("git")
                .args(["rm", "--cached", "-q", "target/source"])
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
        assert!(!has_tracked_files(root, Path::new("target")).unwrap());
        assert_eq!(
            detach_directory(root, Path::new("target"), (0, 0)).unwrap(),
            None
        );
        let outside = root.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), "outside").unwrap();
        symlink(&outside, root.join("target/link")).unwrap();
        assert!(confined_directory(&SystemCacheTree, root, Path::new("target/link")).is_err());
        assert!(
            session_cache::measure(&SystemCacheTree, &root.join("target"))
                .unwrap()
                .bytes
                > 0
        );
        fs::create_dir(root.join(".usagi")).unwrap();
        symlink(&outside, root.join(".usagi/cache-trash")).unwrap();
        assert!(detach_directory(root, Path::new("target"), identity).is_err());
        fs::remove_file(root.join(".usagi/cache-trash")).unwrap();
        let detached = detach_directory(root, Path::new("target"), identity)
            .unwrap()
            .unwrap();
        assert!(!root.join("target").exists());
        fs::remove_dir_all(detached).unwrap();
        assert_eq!(fs::read_to_string(outside.join("keep")).unwrap(), "outside");
        assert_eq!(
            confined_directory(&SystemCacheTree, root, Path::new("")).unwrap(),
            root
        );
        assert!(session_directory(&SystemCacheTree, root, &outside).is_err());
        fs::create_dir(root.join(".usagi/sessions")).unwrap();
        symlink(&outside, root.join(".usagi/sessions/escaped")).unwrap();
        assert!(
            session_directory(
                &SystemCacheTree,
                root,
                &root.join(".usagi/sessions/escaped")
            )
            .is_err()
        );
        fs::create_dir(root.join(".usagi/sessions/real")).unwrap();
        assert!(
            session_directory(&SystemCacheTree, root, &root.join(".usagi/sessions/real")).is_ok()
        );
        assert!(has_tracked_files(&root.join("missing"), Path::new("target")).is_err());
        fs::create_dir(root.join("build[1]")).unwrap();
        fs::write(root.join("build[1]/source"), "tracked").unwrap();
        assert!(
            Command::new("git")
                .args(["--literal-pathspecs", "add", "build[1]/source"])
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
        assert!(has_tracked_files(root, Path::new("build[1]")).unwrap());
    }

    #[test]
    fn session_cache_activity_protects_root_and_exact_session_scopes() {
        let session = SessionId::new();
        let mut terminal = TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: WorkspaceId::new(),
            session_id: Some(session),
            worktree_id: WorktreeId::new(),
        };
        assert!(!protected(&Vec::new(), session));
        assert!(protected(
            &vec![(terminal.clone(), TerminalRuntimeState::Running)],
            session
        ));
        assert!(!protected(
            &vec![(terminal.clone(), TerminalRuntimeState::Exited)],
            session
        ));
        assert!(!protected(
            &vec![(terminal.clone(), TerminalRuntimeState::Running)],
            SessionId::new()
        ));
        terminal.session_id = None;
        assert!(protected(
            &vec![(terminal, TerminalRuntimeState::Reserved)],
            session
        ));
        assert!(
            !session_error(
                usagi_daemon::usecase::session_runtime::SessionRuntimeError::UnknownSession
            )
            .to_string()
            .is_empty()
        );
    }
}

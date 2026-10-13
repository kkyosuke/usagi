//! Filesystem and runtime-owner adapters for session-cache retention.

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use usagi_core::domain::id::{SessionId, TerminalRef};
use usagi_core::domain::session_cache::SessionCachePolicy;
use usagi_core::infrastructure::git::GitRunner;
use usagi_core::infrastructure::runtime_model::WorkspaceSessionConfig;
use usagi_core::infrastructure::session_snapshot::SessionListSnapshot;
use usagi_daemon::usecase::session_cache::{
    self, CacheCandidate, CacheNode, CacheTreeIo, CacheUsage, SessionCacheIo,
};
use usagi_daemon::usecase::terminal::TerminalRuntimeState;

use super::{ConnectionWorkspace, SharedAgentRuntime, SharedTerminalRuntime, SystemGit};

type Activity = Vec<(TerminalRef, TerminalRuntimeState)>;
type WorkspaceActivity = (Activity, Activity);
type NamedScope = (
    io::Result<usagi_daemon::usecase::session_runtime::SessionScope>,
    String,
);

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

fn same_directory(io: &impl CacheTreeIo, left: &Path, right: &Path) -> io::Result<bool> {
    let left = match io.node(left) {
        Ok(node) => node,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let right = match io.node(right) {
        Ok(node) => node,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    Ok(left.directory && right.directory && left.usage.identity == right.usage.identity)
}

fn contains_executable(
    io: &impl CacheTreeIo,
    directory: &Path,
    executable: &Path,
) -> io::Result<bool> {
    for ancestor in executable.ancestors() {
        if same_directory(io, directory, ancestor)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn has_tracked_files(
    io: &impl CacheTreeIo,
    git: &impl GitRunner,
    root: &Path,
    relative: &Path,
) -> io::Result<bool> {
    // A literal pathspec is still case-sensitive on a case-insensitive filesystem.
    // Compare index spelling with filesystem identity, including tracked gitlinks
    // that are ancestors of the selected directory.
    let result = git
        .run(root, &["ls-files", "-z", "--"])
        .map_err(io::Error::other)?;
    if !result.success {
        return Err(io::Error::other("could not verify cache files against Git"));
    }
    // GitRunner exposes UTF-8 text. Fail closed if its lossy conversion could
    // have changed a non-UTF-8 index filename before filesystem comparison.
    if result.stdout.contains('\u{fffd}') {
        return Err(io::Error::other("could not decode tracked cache paths"));
    }
    let depth = relative.components().count();
    let mut checked = std::collections::BTreeSet::new();
    for tracked in result.stdout.split_terminator('\0').map(Path::new) {
        if tracked.starts_with(relative) || relative.starts_with(tracked) {
            return Ok(true);
        }
        let shared_depth = depth.min(tracked.components().count());
        let cache_prefix: PathBuf = relative.components().take(shared_depth).collect();
        let tracked_prefix: PathBuf = tracked.components().take(shared_depth).collect();
        if checked.insert((cache_prefix.clone(), tracked_prefix.clone()))
            && same_directory(io, &root.join(cache_prefix), &root.join(tracked_prefix))?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn discover_candidates(
    io: &impl CacheTreeIo,
    git: &impl GitRunner,
    workspace: &Path,
    scopes: Vec<NamedScope>,
    paths: &[PathBuf],
    activity: &WorkspaceActivity,
    executable: &Path,
) -> io::Result<Vec<CacheCandidate>> {
    let mut candidates = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for (scope, name) in scopes {
        let Ok(scope) = scope else { continue };
        let Ok(root) = session_directory(io, workspace, &scope.path) else {
            continue;
        };
        for relative in paths {
            let Ok(path) = confined_directory(io, &root, relative) else {
                continue;
            };
            let tracked = has_tracked_files(io, git, &root, relative)?;
            let Ok(usage) = session_cache::measure(io, &path) else {
                continue;
            };
            if !seen.insert(usage.identity) {
                continue;
            }
            candidates.push(CacheCandidate {
                session: scope.session_id,
                session_name: name.clone(),
                path: relative.clone(),
                usage,
                protected: tracked
                    || contains_executable(io, &path, executable)?
                    || protected(&activity.0, scope.session_id)
                    || protected(&activity.1, scope.session_id),
            });
        }
    }
    Ok(candidates)
}

#[derive(Clone)]
struct CacheObservation {
    policy: SessionCachePolicy,
    activity: WorkspaceActivity,
}

struct DetachmentContext<'a> {
    workspace: &'a Path,
    root: &'a Path,
    executable: &'a Path,
    scanned: &'a CacheObservation,
    current: &'a CacheObservation,
}

fn detach_revalidated(
    io: &impl CacheMutationIo,
    git: &impl GitRunner,
    context: &DetachmentContext<'_>,
    candidate: &CacheCandidate,
) -> io::Result<Option<PathBuf>> {
    // Configuration may have been changed or disabled during the slow scan.
    // A process may also have started and exited without remaining live.
    if context.current.policy != context.scanned.policy
        || context.current.activity != context.scanned.activity
        || protected(&context.current.activity.0, candidate.session)
        || protected(&context.current.activity.1, candidate.session)
    {
        return Ok(None);
    }
    let root = session_directory(io, context.workspace, context.root)?;
    detach_directory(
        io,
        git,
        &root,
        &candidate.path,
        candidate.usage.identity,
        context.executable,
    )
}

/// Only filesystem effects remain excluded from coverage; confinement, identity,
/// tracked-file protection and interruption recovery use this injected boundary.
trait CacheMutationIo: CacheTreeIo {
    fn create_directory(&self, path: &Path) -> io::Result<()>;
    fn rename_directory(&self, source: &Path, destination: &Path) -> io::Result<()>;
    fn remove_tree(&self, path: &Path) -> io::Result<()>;
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=session_cache_fs_keeps_tracked_and_symlinked_directories
impl CacheMutationIo for SystemCacheTree {
    fn create_directory(&self, path: &Path) -> io::Result<()> {
        fs::create_dir(path)
    }
    fn rename_directory(&self, source: &Path, destination: &Path) -> io::Result<()> {
        fs::rename(source, destination)
    }
    fn remove_tree(&self, path: &Path) -> io::Result<()> {
        fs::remove_dir_all(path)
    }
}

struct SystemSessionCaches<'a> {
    bound: &'a ConnectionWorkspace,
    agent: &'a SharedAgentRuntime,
    terminal: &'a SharedTerminalRuntime,
    observation: CacheObservation,
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
        self.observation.activity = (
            agent.cache_cleanup_activity(workspace),
            terminal.cache_cleanup_activity(workspace),
        );
        let mut scopes = Vec::new();
        for item in snapshot.sessions {
            scopes.push((
                runtime
                    .session_scope_by_id(item.session.session_id)
                    .map_err(session_error),
                item.session.name,
            ));
        }
        drop(runtime);
        drop(terminal);
        drop(agent);
        discover_candidates(
            &SystemCacheTree,
            &SystemGit,
            self.bound.tenant.root(),
            scopes,
            paths,
            &self.observation.activity,
            &self.executable,
        )
    }

    #[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=session_cache_cleanup_reports_usage_and_keeps_the_session
    fn detach(&mut self, candidate: &CacheCandidate) -> io::Result<Option<PathBuf>> {
        let agent = lock_owner(&self.agent.owner)?;
        let terminal = lock_owner(self.terminal)?;
        let workspace = self.bound.tenant.workspace_id();
        let runtime = lock_owner(self.bound.sessions())?;
        let scope = runtime
            .session_scope_by_id(candidate.session)
            .map_err(session_error)?;
        let current = CacheObservation {
            policy: WorkspaceSessionConfig::load(self.bound.tenant.root())
                .map_err(io::Error::other)?
                .cache_cleanup()
                .clone(),
            activity: (
                agent.cache_cleanup_activity(workspace),
                terminal.cache_cleanup_activity(workspace),
            ),
        };
        detach_revalidated(
            &SystemCacheTree,
            &SystemGit,
            &DetachmentContext {
                workspace: self.bound.tenant.root(),
                root: &scope.path,
                executable: &self.executable,
                scanned: &self.observation,
                current: &current,
            },
            candidate,
        )
    }

    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=session_cache_fs_keeps_tracked_and_symlinked_directories
    fn remove(&mut self, detached: &Path) -> io::Result<()> {
        fs::remove_dir_all(detached)
    }
}

fn detach_directory(
    io: &impl CacheMutationIo,
    git: &impl GitRunner,
    root: &Path,
    relative: &Path,
    identity: (u64, u64),
    executable: &Path,
) -> io::Result<Option<PathBuf>> {
    let path = confined_directory(io, root, relative)?;
    if io.node(&path)?.usage.identity != identity
        || has_tracked_files(io, git, root, relative)?
        || contains_executable(io, &path, executable)?
    {
        return Ok(None);
    }
    // Keep the detached tree in the same filesystem. Renaming is short; slow
    // removal happens after the runtime locks are released.
    let state = root.join(".usagi");
    match io.create_directory(&state) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    confined_directory(io, root, Path::new(".usagi"))?;
    let trash = state.join("cache-trash");
    match io.create_directory(&trash) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    confined_directory(io, &state, Path::new("cache-trash"))?;
    let destination = trash.join(usagi_core::domain::id::OperationId::new().to_string());
    io.rename_directory(&path, &destination)?;
    Ok(Some(destination))
}

trait CacheMaintenanceIo: SessionCacheIo {
    fn generation_state(
        &mut self,
    ) -> io::Result<(
        usize,
        Vec<usagi_daemon::usecase::generation::GenerationRole>,
    )>;
    fn purge_detached(&mut self) -> io::Result<()>;
}

impl CacheMaintenanceIo for SystemSessionCaches<'_> {
    #[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=session_cache_cleanup_reports_usage_and_keeps_the_session
    fn generation_state(
        &mut self,
    ) -> io::Result<(
        usize,
        Vec<usagi_daemon::usecase::generation::GenerationRole>,
    )> {
        let generations = super::trusted_generations()
            .map_err(io::Error::other)?
            .len();
        let data = usagi_core::infrastructure::paths::data_dir().map_err(io::Error::other)?;
        let document =
            usagi_daemon::infrastructure::generation_registry::read_registry_document(&data)
                .map_err(io::Error::other)?
                .ok_or(io::Error::other(
                    "cache cleanup generation ownership is unavailable",
                ))?;
        let mut roles = Vec::new();
        for entry in document.generations {
            roles.push(entry.role);
        }
        Ok((generations, roles))
    }

    #[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=session_cache_cleanup_reports_usage_and_keeps_the_session
    fn purge_detached(&mut self) -> io::Result<()> {
        let runtime = lock_owner(self.bound.sessions())?;
        let snapshot: SessionListSnapshot =
            serde_json::from_value(runtime.snapshot().map_err(session_error)?)?;
        let mut scopes = Vec::new();
        for item in snapshot.sessions {
            scopes.push((
                runtime
                    .session_scope_by_id(item.session.session_id)
                    .map_err(session_error),
                item.session.name,
            ));
        }
        drop(runtime);
        purge_detached_caches(
            &SystemCacheTree,
            &SystemGit,
            self.bound.tenant.root(),
            scopes,
        )
    }
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
        observation: CacheObservation {
            policy: policy.clone(),
            activity: (Vec::new(), Vec::new()),
        },
        executable: std::env::current_exe()?,
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    cleanup_request(&mut io, &policy, now, apply)
}

fn cleanup_request(
    io: &mut impl CacheMaintenanceIo,
    policy: &SessionCachePolicy,
    now: u64,
    apply: bool,
) -> io::Result<serde_json::Value> {
    policy.validate().map_err(io::Error::other)?;
    if apply {
        // Interrupted trash also needs this fence after a policy is disabled.
        let (generations, roles) = io.generation_state()?;
        if generations != 1 || !generations_quiet(&roles) {
            return Err(io::Error::other(
                "cache cleanup waits until daemon handoff finishes",
            ));
        }
        io.purge_detached()?;
        let report = session_cache::clean(io, policy, now)?;
        Ok(
            serde_json::json!({"mode": "apply", "removed": report.removed, "removed_bytes": report.removed_bytes, "deferred": report.deferred}),
        )
    } else {
        let candidates = io.candidates(&policy.paths)?;
        let plan = session_cache::plan(policy, &candidates, now);
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

fn purge_detached_caches(
    io: &impl CacheMutationIo,
    git: &impl GitRunner,
    workspace: &Path,
    scopes: Vec<NamedScope>,
) -> io::Result<()> {
    for (scope, _) in scopes {
        let Ok(scope) = scope else { continue };
        let root = scope.path;
        if session_directory(io, workspace, &root).is_err() {
            continue;
        }
        let Ok(trash) = confined_directory(io, &root, Path::new(".usagi/cache-trash")) else {
            continue;
        };
        for entry in io.children(&trash)? {
            let Some(name) = entry.file_name() else {
                continue;
            };
            let Some(name) = name.to_str() else {
                continue;
            };
            if usagi_core::domain::id::OperationId::parse(name).is_err() {
                continue;
            }
            if !has_tracked_files(io, git, &root, &Path::new(".usagi/cache-trash").join(name))? {
                io.remove_tree(&entry)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};
    use std::os::unix::fs::symlink;
    use std::process::Command;
    use usagi_core::domain::id::{DaemonGeneration, TerminalId, WorkspaceId, WorktreeId};

    use super::*;

    fn has_tracked_files(root: &Path, relative: &Path) -> io::Result<bool> {
        super::has_tracked_files(&SystemCacheTree, &SystemGit, root, relative)
    }

    fn detach_directory(
        root: &Path,
        relative: &Path,
        identity: (u64, u64),
    ) -> io::Result<Option<PathBuf>> {
        super::detach_directory(
            &SystemCacheTree,
            &SystemGit,
            root,
            relative,
            identity,
            Path::new("/unrelated-usagi-cache-test-executable"),
        )
    }

    #[derive(Default)]
    struct FakeTree {
        nodes: RefCell<BTreeMap<PathBuf, CacheNode>>,
        unreadable: BTreeSet<PathBuf>,
        unreadable_children: BTreeSet<PathBuf>,
        create_failures: BTreeSet<PathBuf>,
        rename_failure: bool,
        remove_failure: bool,
        extra_children: Vec<PathBuf>,
        removed: RefCell<Vec<PathBuf>>,
    }

    impl FakeTree {
        fn directory(&self, path: &str, inode: u64) {
            self.nodes.borrow_mut().insert(
                path.into(),
                CacheNode {
                    directory: true,
                    symlink: false,
                    usage: CacheUsage {
                        bytes: 10,
                        modified: 1,
                        identity: (1, inode),
                    },
                },
            );
        }

        fn fixture() -> Self {
            let tree = Self::default();
            for (index, path) in [
                "/workspace/.usagi",
                "/workspace/.usagi/sessions",
                "/workspace/.usagi/sessions/fixture",
                "/workspace/.usagi/sessions/fixture/target",
            ]
            .into_iter()
            .enumerate()
            {
                tree.directory(path, index as u64 + 1);
            }
            tree
        }
    }

    impl CacheTreeIo for FakeTree {
        fn node(&self, path: &Path) -> io::Result<CacheNode> {
            if self.unreadable.contains(path) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "unreadable fixture",
                ));
            }
            self.nodes
                .borrow()
                .get(path)
                .copied()
                .ok_or(io::Error::new(io::ErrorKind::NotFound, "absent fixture"))
        }

        fn children(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
            if self.unreadable_children.contains(path) {
                return Err(io::Error::other("incomplete listing"));
            }
            let mut children = self
                .nodes
                .borrow()
                .keys()
                .filter(|entry| entry.parent() == Some(path))
                .cloned()
                .collect::<Vec<_>>();
            children.extend(self.extra_children.clone());
            Ok(children)
        }
    }

    impl CacheMutationIo for FakeTree {
        fn create_directory(&self, path: &Path) -> io::Result<()> {
            if self.create_failures.contains(path) {
                return Err(io::Error::other("creation failed"));
            }
            if self.nodes.borrow().contains_key(path) {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "exists"));
            }
            self.directory(path.to_str().unwrap(), 100);
            Ok(())
        }
        fn rename_directory(&self, _: &Path, _: &Path) -> io::Result<()> {
            if self.rename_failure {
                Err(io::Error::other("rename failed"))
            } else {
                Ok(())
            }
        }
        fn remove_tree(&self, path: &Path) -> io::Result<()> {
            if self.remove_failure {
                return Err(io::Error::other("removal failed"));
            }
            self.removed.borrow_mut().push(path.into());
            Ok(())
        }
    }

    fn scope(path: &str) -> usagi_daemon::usecase::session_runtime::SessionScope {
        usagi_daemon::usecase::session_runtime::SessionScope {
            workspace_id: WorkspaceId::new(),
            session_id: SessionId::new(),
            worktree_id: WorktreeId::new(),
            path: path.into(),
        }
    }

    fn policy() -> SessionCachePolicy {
        SessionCachePolicy {
            paths: vec!["target".into()],
            max_bytes: Some(0),
            max_age_hours: None,
        }
    }

    #[derive(Default)]
    struct FakeGit {
        files: String,
        failure: u8,
    }
    impl GitRunner for FakeGit {
        fn run(
            &self,
            _: &Path,
            args: &[&str],
        ) -> anyhow::Result<usagi_core::infrastructure::git::GitOutput> {
            assert_eq!(args, ["ls-files", "-z", "--"]);
            if self.failure == 1 {
                return Err(anyhow::anyhow!("git unavailable"));
            }
            Ok(usagi_core::infrastructure::git::GitOutput {
                success: self.failure != 2,
                stdout: self.files.clone(),
                stderr: String::new(),
            })
        }
    }
    fn git(files: &str) -> FakeGit {
        FakeGit {
            files: files.into(),
            failure: 0,
        }
    }

    #[test]
    fn session_cache_index_protects_filesystem_aliases_and_gitlink_descendants() {
        let root = Path::new("/fixture");
        let mut tree = FakeTree::default();
        tree.directory("/fixture/target", 1);
        tree.directory("/fixture/TARGET", 1);
        tree.directory("/fixture/café", 2);
        tree.directory("/fixture/cafe\u{301}", 2);
        for (cache, tracked) in [
            ("TARGET", "target/source\0target/other\0"),
            ("cafe\u{301}", "café/source\0"),
            ("TARGET/cache", "target\0"),
            ("target", "target/missing\0"),
        ] {
            assert!(
                super::has_tracked_files(&tree, &git(tracked), root, Path::new(cache)).unwrap()
            );
        }
        tree.directory("/fixture/elsewhere", 3);
        assert!(
            !super::has_tracked_files(
                &tree,
                &git("elsewhere/a\0elsewhere/b\0missing/file\0"),
                root,
                Path::new("target")
            )
            .unwrap()
        );
        assert!(!same_directory(&tree, Path::new("/fixture/missing"), root).unwrap());
        assert!(!same_directory(&tree, root, Path::new("/fixture/missing")).unwrap());
        tree.unreadable.insert("/fixture/target".into());
        assert!(same_directory(&tree, Path::new("/fixture/target"), root).is_err());
        assert!(
            same_directory(
                &tree,
                Path::new("/fixture/elsewhere"),
                Path::new("/fixture/target")
            )
            .is_err()
        );
        assert!(
            super::has_tracked_files(&tree, &git("elsewhere/file\0"), root, Path::new("target"))
                .is_err()
        );
        assert!(
            super::has_tracked_files(&tree, &git("\u{fffd}/file\0"), root, Path::new("target"))
                .is_err()
        );
        for failure in [1, 2] {
            assert!(
                super::has_tracked_files(
                    &tree,
                    &FakeGit {
                        failure,
                        ..FakeGit::default()
                    },
                    root,
                    Path::new("target")
                )
                .is_err()
            );
        }
    }

    #[test]
    fn session_cache_discovery_keeps_aliases_executables_and_incomplete_trees_safe() {
        let mut tree = FakeTree::fixture();
        let root = "/workspace/.usagi/sessions/fixture";
        tree.directory(&format!("{root}/TARGET"), 4);
        let scopes = || {
            vec![
                (Err(io::Error::other("unavailable")), "unavailable".into()),
                (Ok(scope("/elsewhere")), "elsewhere".into()),
                (Ok(scope(root)), "fixture".into()),
            ]
        };
        let activity = (Vec::new(), Vec::new());
        let paths = ["missing", "target", "TARGET"].map(PathBuf::from);
        let entries = discover_candidates(
            &tree,
            &git(""),
            Path::new("/workspace"),
            scopes(),
            &paths,
            &activity,
            Path::new("/bin/usagi"),
        )
        .unwrap();
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].protected);
        assert_eq!(entries[0].session_name, "fixture");
        let executable = PathBuf::from(format!("{root}/TARGET/usagi"));
        assert!(
            discover_candidates(
                &tree,
                &git(""),
                Path::new("/workspace"),
                scopes(),
                &paths,
                &activity,
                &executable,
            )
            .unwrap()[0]
                .protected
        );
        assert!(
            discover_candidates(
                &tree,
                &git("target/source\0"),
                Path::new("/workspace"),
                scopes(),
                &paths,
                &activity,
                Path::new("/bin/usagi"),
            )
            .unwrap()[0]
                .protected
        );
        assert!(
            discover_candidates(
                &tree,
                &FakeGit {
                    failure: 1,
                    ..FakeGit::default()
                },
                Path::new("/workspace"),
                scopes(),
                &paths,
                &activity,
                &executable,
            )
            .is_err()
        );
        tree.unreadable_children
            .insert(format!("{root}/target").into());
        assert!(
            discover_candidates(
                &tree,
                &git(""),
                Path::new("/workspace"),
                scopes(),
                &paths[..2],
                &activity,
                &executable,
            )
            .unwrap()
            .is_empty()
        );
        tree.unreadable_children.clear();
        tree.unreadable.insert(executable);
        assert!(
            discover_candidates(
                &tree,
                &git(""),
                Path::new("/workspace"),
                scopes(),
                &paths,
                &activity,
                Path::new(&format!("{root}/TARGET/usagi")),
            )
            .is_err()
        );
    }

    fn fixture_candidate() -> CacheCandidate {
        CacheCandidate {
            session: SessionId::new(),
            session_name: "fixture".into(),
            path: "target".into(),
            usage: CacheUsage {
                bytes: 10,
                modified: 1,
                identity: (1, 4),
            },
            protected: false,
        }
    }

    fn activity(session: Option<SessionId>, state: TerminalRuntimeState) -> Activity {
        vec![(
            TerminalRef {
                daemon_generation: DaemonGeneration::new(),
                terminal_id: TerminalId::new(),
                workspace_id: WorkspaceId::new(),
                session_id: session,
                worktree_id: WorktreeId::new(),
            },
            state,
        )]
    }

    #[test]
    fn session_cache_detachment_rechecks_policy_activity_and_directory_identity() {
        let tree = FakeTree::fixture();
        let root = Path::new("/workspace/.usagi/sessions/fixture");
        let candidate = fixture_candidate();
        let scanned = CacheObservation {
            policy: policy(),
            activity: (Vec::new(), Vec::new()),
        };
        let mut current = scanned.clone();
        let recheck = |current: &CacheObservation, candidate: &CacheCandidate| {
            detach_revalidated(
                &tree,
                &git(""),
                &DetachmentContext {
                    workspace: Path::new("/workspace"),
                    root,
                    executable: Path::new("/bin/usagi"),
                    scanned: &scanned,
                    current,
                },
                candidate,
            )
        };
        for changed in [
            SessionCachePolicy::default(),
            SessionCachePolicy {
                paths: vec!["other".into()],
                ..policy()
            },
            SessionCachePolicy {
                max_bytes: Some(100),
                ..policy()
            },
            SessionCachePolicy {
                max_age_hours: Some(24),
                ..policy()
            },
        ] {
            current.policy = changed;
            assert_eq!(recheck(&current, &candidate).unwrap(), None);
        }
        current = scanned.clone();
        current.activity.0 = activity(Some(candidate.session), TerminalRuntimeState::Exited);
        assert_eq!(recheck(&current, &candidate).unwrap(), None);
        assert!(recheck(&scanned, &candidate).unwrap().is_some());
        let mut replaced = candidate.clone();
        replaced.usage.identity = (0, 0);
        assert_eq!(recheck(&scanned, &replaced).unwrap(), None);
        for owners in [
            (
                activity(Some(candidate.session), TerminalRuntimeState::Running),
                Vec::new(),
            ),
            (Vec::new(), activity(None, TerminalRuntimeState::Reserved)),
        ] {
            let busy = CacheObservation {
                policy: policy(),
                activity: owners,
            };
            assert_eq!(
                detach_revalidated(
                    &tree,
                    &git(""),
                    &DetachmentContext {
                        workspace: Path::new("/workspace"),
                        root,
                        executable: Path::new("/bin/usagi"),
                        scanned: &busy,
                        current: &busy,
                    },
                    &candidate
                )
                .unwrap(),
                None
            );
        }
        assert!(
            detach_revalidated(
                &tree,
                &git(""),
                &DetachmentContext {
                    workspace: Path::new("/workspace"),
                    root: Path::new("/elsewhere"),
                    executable: Path::new("/bin/usagi"),
                    scanned: &scanned,
                    current: &scanned,
                },
                &candidate
            )
            .is_err()
        );
    }

    #[test]
    fn session_cache_detachment_defers_filesystem_and_index_failures() {
        let root = Path::new("/workspace/.usagi/sessions/fixture");
        let detach = |tree: &FakeTree, git: &FakeGit, executable: &Path| {
            super::detach_directory(tree, git, root, Path::new("target"), (1, 4), executable)
        };
        let external = Path::new("/bin/usagi");
        let mut tree = FakeTree::fixture();
        assert_eq!(
            detach(&tree, &git("target/source\0"), external).unwrap(),
            None
        );
        assert_eq!(
            detach(&tree, &git(""), &root.join("target/usagi")).unwrap(),
            None
        );
        assert!(
            detach(
                &tree,
                &FakeGit {
                    failure: 1,
                    ..FakeGit::default()
                },
                external
            )
            .is_err()
        );
        for directory in [root.join(".usagi"), root.join(".usagi/cache-trash")] {
            tree.create_failures.insert(directory.clone());
            assert!(detach(&tree, &git(""), external).is_err());
            tree.create_failures.remove(&directory);
        }
        tree.rename_failure = true;
        assert!(detach(&tree, &git(""), external).is_err());
        tree.rename_failure = false;
        assert!(detach(&tree, &git(""), external).unwrap().is_some());
        tree.nodes
            .borrow_mut()
            .get_mut(&root.join("target"))
            .unwrap()
            .symlink = true;
        assert!(detach(&tree, &git(""), external).is_err());
    }

    #[test]
    fn session_cache_trash_recovery_checks_scope_names_and_tracked_files() {
        use std::os::unix::ffi::OsStringExt;
        let mut tree = FakeTree::fixture();
        let root = "/workspace/.usagi/sessions/fixture";
        let scopes = || {
            vec![
                (Err(io::Error::other("unavailable")), "unavailable".into()),
                (Ok(scope("/elsewhere")), "elsewhere".into()),
                (Ok(scope(root)), "fixture".into()),
            ]
        };
        let purge = |tree: &FakeTree, git: &FakeGit| {
            purge_detached_caches(tree, git, Path::new("/workspace"), scopes())
        };
        assert!(purge(&tree, &git("")).is_ok());
        let trash = format!("{root}/.usagi/cache-trash");
        tree.directory(&format!("{root}/.usagi"), 10);
        tree.directory(&trash, 11);
        let removable = usagi_core::domain::id::OperationId::new().to_string();
        let retained = usagi_core::domain::id::OperationId::new().to_string();
        let removable_path = PathBuf::from(format!("{trash}/{removable}"));
        tree.directory(removable_path.to_str().unwrap(), 12);
        tree.directory(&format!("{trash}/{retained}"), 13);
        tree.directory(&format!("{trash}/unowned"), 14);
        tree.extra_children = vec![
            PathBuf::from("/"),
            PathBuf::from(std::ffi::OsString::from_vec(vec![0xff])),
        ];
        let tracked = git(&format!(".usagi/cache-trash/{retained}/source\0"));
        assert!(purge(&tree, &tracked).is_ok());
        assert_eq!(*tree.removed.borrow(), [removable_path]);
        tree.remove_failure = true;
        assert!(purge(&tree, &tracked).is_err());
        tree.remove_failure = false;
        tree.unreadable_children.insert(trash.into());
        assert!(purge(&tree, &tracked).is_err());
        tree.unreadable_children.clear();
        assert!(
            purge(
                &tree,
                &FakeGit {
                    failure: 1,
                    ..FakeGit::default()
                }
            )
            .is_err()
        );
    }

    struct FakeMaintenance {
        entries: Vec<CacheCandidate>,
        failure: u8,
        generations: usize,
        roles: Vec<usagi_daemon::usecase::generation::GenerationRole>,
        purged: bool,
    }
    impl Default for FakeMaintenance {
        fn default() -> Self {
            Self {
                entries: vec![fixture_candidate()],
                failure: 0,
                generations: 1,
                roles: vec![usagi_daemon::usecase::generation::GenerationRole::Active],
                purged: false,
            }
        }
    }
    impl SessionCacheIo for FakeMaintenance {
        fn candidates(&mut self, paths: &[PathBuf]) -> io::Result<Vec<CacheCandidate>> {
            if self.failure == 3 {
                return Err(io::Error::other("discovery failed"));
            }
            Ok(if paths.is_empty() {
                Vec::new()
            } else {
                self.entries.clone()
            })
        }
        fn detach(&mut self, candidate: &CacheCandidate) -> io::Result<Option<PathBuf>> {
            if self.failure == 4 {
                return Err(io::Error::other("detachment failed"));
            }
            Ok(Some(candidate.path.clone()))
        }
        fn remove(&mut self, _: &Path) -> io::Result<()> {
            if self.failure == 5 {
                Err(io::Error::other("removal failed"))
            } else {
                Ok(())
            }
        }
    }
    impl CacheMaintenanceIo for FakeMaintenance {
        fn generation_state(
            &mut self,
        ) -> io::Result<(
            usize,
            Vec<usagi_daemon::usecase::generation::GenerationRole>,
        )> {
            if self.failure == 1 {
                return Err(io::Error::other("generation unavailable"));
            }
            Ok((self.generations, self.roles.clone()))
        }
        fn purge_detached(&mut self) -> io::Result<()> {
            self.purged = true;
            if self.failure == 2 {
                Err(io::Error::other("purge failed"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn session_cache_request_reports_results_and_fences_disabled_policy_trash() {
        let mut io = FakeMaintenance::default();
        let dry = cleanup_request(&mut io, &policy(), 10, false).unwrap();
        assert_eq!(dry["mode"], "dry_run");
        assert_eq!(dry["candidates"][0]["selected"], true);
        assert_eq!(dry["total_bytes"], 10);
        assert!(!io.purged);
        let disabled = cleanup_request(&mut io, &SessionCachePolicy::default(), 10, false).unwrap();
        assert_eq!(disabled["enabled"], false);
        assert_eq!(disabled["total_bytes"], 0);
        assert_eq!(
            cleanup_request(&mut io, &policy(), 10, true).unwrap()["removed"],
            1
        );
        assert!(io.purged);
        for failure in 1..=5 {
            io.failure = failure;
            let result = cleanup_request(&mut io, &policy(), 10, true);
            if failure <= 3 {
                assert!(result.is_err());
            } else {
                assert_eq!(result.unwrap()["deferred"], 1);
            }
        }
        io.failure = 3;
        assert!(cleanup_request(&mut io, &policy(), 10, false).is_err());
        io.failure = 0;
        io.generations = 2;
        io.purged = false;
        assert!(cleanup_request(&mut io, &SessionCachePolicy::default(), 10, true).is_err());
        assert!(!io.purged);
        io.generations = 1;
        io.roles
            .push(usagi_daemon::usecase::generation::GenerationRole::Draining);
        assert!(cleanup_request(&mut io, &policy(), 10, true).is_err());
        let invalid = SessionCachePolicy {
            paths: vec!["..".into()],
            ..policy()
        };
        assert!(cleanup_request(&mut io, &invalid, 10, false).is_err());
    }

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
    fn session_cache_filesystem_aliases_protect_index_and_executable() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        let run_git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(root)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        run_git(&["init", "-q"]);
        for (name, alias) in [("target", "TARGET"), ("café", "cafe\u{301}")] {
            fs::create_dir(root.join(name)).unwrap();
            fs::write(root.join(name).join("source"), "retained").unwrap();
            let source = format!("{name}/source");
            run_git(&["add", &source]);
            let selected = if root.join(alias).is_dir() {
                alias
            } else {
                name
            };
            let identity = SystemCacheTree
                .node(&root.join(selected))
                .unwrap()
                .usage
                .identity;
            assert!(has_tracked_files(root, Path::new(selected)).unwrap());
            assert_eq!(
                detach_directory(root, Path::new(selected), identity).unwrap(),
                None
            );
            run_git(&["rm", "--cached", "-q", &source]);
            assert_eq!(
                super::detach_directory(
                    &SystemCacheTree,
                    &SystemGit,
                    root,
                    Path::new(selected),
                    identity,
                    &root.join(name).join("usagi"),
                )
                .unwrap(),
                None
            );
            assert_eq!(
                fs::read_to_string(root.join(name).join("source")).unwrap(),
                "retained"
            );
        }
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

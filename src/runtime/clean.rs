//! Production IO adapter for `usagi clean`.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use fs2::FileExt;
use usagi_core::domain::daemon::DaemonRecord;
use usagi_core::infrastructure::git::{
    GitOutput, GitRunner, confined_git_command, delete_branch, list_worktrees, remove_worktree,
};
use usagi_core::infrastructure::paths::{self, SESSIONS_DIR, STATE_DIR};
use usagi_core::infrastructure::store::lifecycle::DaemonLifecycleStore;
use usagi_core::infrastructure::store::workspace::Storage;
use usagi_core::infrastructure::workspace_state;
use usagi_core::usecase::clean::{
    CleanCandidate, CleanInventory, DaemonWorkspaceData, HelperRole, ObservedBranch,
    ObservedProcess, ObservedWorktree, RegisteredWorkspace, RepositoryInventory,
};
use usagi_daemon::infrastructure::unix_transport::ensure_private_dir;

/// Discover and optionally remove orphan resources. Discovery is always done
/// first, so an apply run prints the exact same plan it is about to execute.
#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=clean_planner_classifies_all_effects
pub(crate) fn run(
    out: &mut dyn Write,
    err: &mut dyn Write,
    apply: bool,
    force: bool,
) -> io::Result<ExitCode> {
    let discovery = discover()?;
    for warning in &discovery.warnings {
        writeln!(err, "clean: incomplete inventory: {warning}")?;
    }
    let candidates = usagi_core::usecase::clean::plan(&discovery.inventory);
    if candidates.is_empty() {
        if discovery.warnings.is_empty() {
            writeln!(out, "clean: no unlinked resources found")?;
        } else {
            writeln!(
                out,
                "clean: no removable resources found; inventory was incomplete"
            )?;
        }
        return Ok(clean_exit_code(discovery.warnings.len()));
    }

    writeln!(
        out,
        "clean: {} unlinked resource(s){}",
        candidates.len(),
        if apply { "" } else { " (dry-run)" }
    )?;
    for candidate in &candidates {
        writeln!(out, "  {}", describe(candidate))?;
    }
    if !apply {
        writeln!(out, "run `usagi clean --apply` to remove safe candidates")?;
        if candidates.iter().any(CleanCandidate::requires_force) {
            writeln!(
                out,
                "run `usagi clean --apply --force` to also remove protected Git candidates"
            )?;
        }
        return Ok(clean_exit_code(discovery.warnings.len()));
    }

    let storage = Storage::open_default().map_err(io::Error::other)?;
    let mut removed = 0usize;
    let mut protected = 0usize;
    let mut failed = discovery.warnings.len();
    for candidate in &candidates {
        if candidate.requires_force() && !force {
            writeln!(err, "clean: skipped protected {}", describe(candidate))?;
            protected += 1;
            continue;
        }
        match apply_candidate(candidate, &storage, force) {
            Ok(()) => removed += 1,
            Err(error) => {
                writeln!(err, "clean: failed {}: {error}", describe(candidate))?;
                failed += 1;
            }
        }
    }
    writeln!(
        out,
        "clean: removed {removed}, protected {protected}, failed {failed}"
    )?;
    Ok(clean_exit_code(failed))
}

fn clean_exit_code(failed: usize) -> ExitCode {
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

struct Discovery {
    inventory: CleanInventory,
    warnings: Vec<String>,
}

#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=clean_planner_classifies_all_effects
fn discover() -> io::Result<Discovery> {
    let storage = Storage::open_default().map_err(io::Error::other)?;
    let registered = storage
        .load_workspaces()
        .map_err(io::Error::other)?
        .into_iter()
        .map(|workspace| RegisteredWorkspace {
            exists: path_node_may_exist(&workspace.path),
            path: workspace.path,
        })
        .collect();
    let daemon_dir = paths::data_dir().map_err(io::Error::other)?.join("daemon");
    let states = workspace_state::adopted(&daemon_dir).map_err(io::Error::other)?;
    let mut daemon_data = Vec::with_capacity(states.len());
    let mut repositories = Vec::new();
    let mut warnings = Vec::new();
    let git = SystemGit;
    for state in states {
        // A broken symlink, file, unreadable node, or non-canonical spelling is
        // not a missing workspace. It may need operator repair, but it does not
        // authorize deleting the state subtree that explains what it was.
        let root_exists = path_node_may_exist(state.root());
        let trusted_repository = state.root().is_absolute()
            && state.root().is_dir()
            && std::fs::canonicalize(state.root()).is_ok_and(|root| root == state.root());
        let mut sessions = match DaemonLifecycleStore::new(state.dir()).load() {
            Ok(Some(lifecycle)) => Some(
                lifecycle
                    .sessions
                    .into_iter()
                    .map(|session| session.name)
                    .collect::<BTreeSet<_>>(),
            ),
            Ok(None) => {
                warnings.push(format!(
                    "{} has no lifecycle document",
                    state.root().display()
                ));
                None
            }
            Err(error) => {
                warnings.push(format!(
                    "{} lifecycle could not be read: {error}",
                    state.root().display()
                ));
                None
            }
        };
        if !state.root().is_absolute() {
            warnings.push(format!(
                "{} records a non-absolute workspace root",
                state.root().display()
            ));
            sessions = None;
        }
        daemon_data.push(DaemonWorkspaceData {
            root: state.root().to_path_buf(),
            dir: state.dir().to_path_buf(),
            root_exists,
            sessions,
        });
        if root_exists && !trusted_repository {
            warnings.push(format!(
                "{} is not a canonical repository directory",
                state.root().display()
            ));
        } else if trusted_repository {
            match discover_repository(&git, state.root()) {
                Ok(Some(repository)) => repositories.push(repository),
                Ok(None) => {}
                Err(error) => warnings.push(format!(
                    "{} Git inventory failed: {error}",
                    state.root().display()
                )),
            }
        }
    }
    Ok(Discovery {
        inventory: CleanInventory {
            processes: discover_processes(&daemon_dir),
            registered,
            daemon_data,
            repositories,
        },
        warnings,
    })
}

#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=clean_planner_classifies_all_effects
fn discover_repository(
    git: &dyn GitRunner,
    root: &Path,
) -> io::Result<Option<RepositoryInventory>> {
    let probe = git
        .run(root, &["rev-parse", "--is-inside-work-tree"])
        .map_err(io::Error::other)?;
    if !probe.success || probe.stdout.trim() != "true" {
        return Ok(None);
    }
    let expected_parent = root.join(STATE_DIR).join(SESSIONS_DIR);
    let mut worktrees = Vec::new();
    for worktree in list_worktrees(git, root).map_err(io::Error::other)? {
        if worktree.path.parent() != Some(expected_parent.as_path()) {
            continue;
        }
        let status = git
            .run(&worktree.path, &["status", "--porcelain"])
            .map_err(io::Error::other)?;
        worktrees.push(ObservedWorktree {
            path: worktree.path,
            dirty: !status.success || !status.stdout.trim().is_empty() || worktree.branch.is_none(),
            branch: worktree.branch,
        });
    }
    let refs = git
        .run(
            root,
            &[
                "for-each-ref",
                "--format=%(refname:short)",
                "refs/heads/usagi/",
            ],
        )
        .map_err(io::Error::other)?;
    if !refs.success {
        return Err(io::Error::other(format!(
            "git branch inventory failed: {}",
            refs.stderr.trim()
        )));
    }
    let mut branches = Vec::new();
    for name in refs
        .stdout
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        let merged = git
            .run(root, &["merge-base", "--is-ancestor", name, "HEAD"])
            .map_err(io::Error::other)?
            .success;
        branches.push(ObservedBranch {
            name: name.to_owned(),
            merged,
        });
    }
    Ok(Some(RepositoryInventory {
        root: root.to_path_buf(),
        worktrees,
        branches,
    }))
}

#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=clean_planner_classifies_all_effects
fn apply_candidate(candidate: &CleanCandidate, storage: &Storage, force: bool) -> io::Result<()> {
    match candidate {
        CleanCandidate::Workspace { path } => {
            if path.exists() {
                return Err(io::Error::other("workspace path exists again"));
            }
            usagi_core::usecase::workspace::remove(storage, std::slice::from_ref(path))
                .map_err(io::Error::other)?;
            Ok(())
        }
        CleanCandidate::Data { root, dir } => {
            let daemon_dir = paths::data_dir().map_err(io::Error::other)?.join("daemon");
            let _daemon_fence = acquire_daemon_fence(&daemon_dir)?;
            ensure_data_unlinked(&daemon_dir, root, dir)?;
            remove_daemon_data(&daemon_dir.join(paths::WORKSPACE_STATE_DIR), dir)
        }
        CleanCandidate::Worktree { root, path, .. } => {
            let _fence = acquire_workspace_fence(root)?;
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| io::Error::other("worktree has no canonical session name"))?;
            ensure_unlinked(root, name)?;
            ensure_managed_worktree(&SystemGit, root, path, name)?;
            remove_worktree(&SystemGit, root, path, force).map_err(io::Error::other)
        }
        CleanCandidate::Branch { root, name, .. } => {
            let _fence = acquire_workspace_fence(root)?;
            let session = name
                .strip_prefix("usagi/")
                .ok_or_else(|| io::Error::other("branch is outside the usagi namespace"))?;
            ensure_unlinked(root, session)?;
            delete_branch(&SystemGit, root, name, force).map_err(io::Error::other)
        }
        CleanCandidate::Process {
            pid,
            start_identity,
            ..
        } => reap_helper_process(*pid, start_identity),
    }
}

/// End one helper process, fenced on the identity observed when it was
/// discovered.
///
/// SIGTERM first so a daemon still gets to close its PTYs, then SIGKILL for one
/// that does not answer. Both signals re-verify the process-start identity, so a
/// pid the OS has handed to someone else between discovery and now is never the
/// one that receives them.
#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=clean_planner_classifies_all_effects
fn reap_helper_process(pid: u32, start_identity: &str) -> io::Result<()> {
    let record = DaemonRecord::identified(pid, start_identity.to_owned());
    if helper_process_gone(pid, start_identity) {
        return Ok(());
    }
    crate::runtime::daemon::signal_exact_process(&record, libc::SIGTERM)?;
    for _ in 0..HELPER_REAP_POLLS {
        if helper_process_gone(pid, start_identity) {
            return Ok(());
        }
        std::thread::sleep(HELPER_REAP_POLL);
    }
    crate::runtime::daemon::signal_exact_process(&record, libc::SIGKILL)?;
    for _ in 0..HELPER_REAP_POLLS {
        if helper_process_gone(pid, start_identity) {
            return Ok(());
        }
        std::thread::sleep(HELPER_REAP_POLL);
    }
    Err(io::Error::other(format!(
        "helper process {pid} survived SIGKILL"
    )))
}

/// Whether the exact process discovered under `pid` is gone. A pid that now
/// carries a different start identity counts as gone: the process this plan
/// meant to end no longer exists.
#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=clean_planner_classifies_all_effects
fn helper_process_gone(pid: u32, start_identity: &str) -> bool {
    !crate::runtime::daemon::process_start_identity(pid)
        .is_ok_and(|observed| observed == start_identity)
}

#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=daemon_data_cleanup_revalidates_binding_and_containment
fn ensure_data_unlinked(daemon_dir: &Path, root: &Path, dir: &Path) -> io::Result<()> {
    if !root.is_absolute() || path_node_may_exist(root) {
        return Err(io::Error::other(
            "workspace root exists again or has an untrusted spelling",
        ));
    }
    let bound = workspace_state::adopted(daemon_dir)
        .map_err(io::Error::other)?
        .into_iter()
        .any(|state| state.root() == root && state.dir() == dir);
    if !bound {
        return Err(io::Error::other(
            "daemon data no longer records the discovered workspace root",
        ));
    }
    DaemonLifecycleStore::new(dir)
        .load()
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::other("workspace lifecycle document is unavailable"))?;
    Ok(())
}

fn path_node_may_exist(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) => error.kind() != io::ErrorKind::NotFound,
    }
}

fn ensure_managed_worktree(
    git: &dyn GitRunner,
    root: &Path,
    path: &Path,
    name: &str,
) -> io::Result<()> {
    let current = list_worktrees(git, root).map_err(io::Error::other)?;
    let Some(worktree) = current.into_iter().find(|worktree| worktree.path == path) else {
        return Ok(());
    };
    let expected = format!("usagi/{name}");
    if worktree
        .branch
        .as_deref()
        .is_some_and(|branch| branch != expected)
    {
        return Err(io::Error::other(
            "worktree branch identity changed after discovery",
        ));
    }
    Ok(())
}

fn remove_daemon_data(container: &Path, dir: &Path) -> io::Result<()> {
    if dir.parent() != Some(container) {
        return Err(io::Error::other(
            "daemon data target is outside the workspace-state container",
        ));
    }
    let metadata = std::fs::symlink_metadata(dir)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::other(
            "daemon data target is not a real directory",
        ));
    }
    std::fs::remove_dir_all(dir)
}

#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=clean_planner_classifies_all_effects
fn ensure_unlinked(root: &Path, name: &str) -> io::Result<()> {
    let daemon_dir = paths::data_dir().map_err(io::Error::other)?.join("daemon");
    let state = workspace_state::adopted(&daemon_dir)
        .map_err(io::Error::other)?
        .into_iter()
        .find(|state| state.root() == root)
        .ok_or_else(|| io::Error::other("workspace lifecycle state is unavailable"))?;
    let lifecycle = DaemonLifecycleStore::new(state.dir())
        .load()
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::other("workspace lifecycle document is unavailable"))?;
    if lifecycle
        .sessions
        .iter()
        .any(|session| session.name == name)
    {
        return Err(io::Error::other("resource became linked to a live session"));
    }
    Ok(())
}

#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=cleanup_fence_rejects_contention_and_insecure_nodes
fn acquire_workspace_fence(root: &Path) -> io::Result<File> {
    let path = paths::workspace_fence_path(root);
    acquire_exclusive_fence(
        &path,
        "workspace is owned by a running daemon; stop it and retry",
    )
}

#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=cleanup_fence_rejects_contention_and_insecure_nodes
fn acquire_daemon_fence(daemon_dir: &Path) -> io::Result<File> {
    let path = daemon_dir.join("daemon.lock");
    acquire_exclusive_fence(&path, "daemon is running; stop it and retry data cleanup")
}

#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=cleanup_fence_rejects_contention_and_insecure_nodes
fn acquire_exclusive_fence(path: &Path, contention: &str) -> io::Result<File> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("cleanup fence has no parent"))?;
    ensure_private_fence_dir(parent)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    verify_fence_node(path, &file)?;
    file.try_lock_exclusive()
        .map_err(|_| io::Error::other(contention))?;
    // A pathname replacement after `open` would put this process and the
    // daemon on different inodes. Verify again after flock, while the held fd
    // still identifies the node whose lock we actually own.
    verify_fence_node(path, &file)?;
    Ok(file)
}

#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=cleanup_fence_rejects_contention_and_insecure_nodes
fn ensure_private_fence_dir(dir: &Path) -> io::Result<()> {
    ensure_private_dir(dir)
}

#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=cleanup_fence_rejects_contention_and_insecure_nodes
fn verify_fence_node(path: &Path, file: &File) -> io::Result<()> {
    let mut held = file.metadata()?;
    let mut named = std::fs::symlink_metadata(path)?;
    if !held.is_file() || !named.is_file() || named.file_type().is_symlink() {
        return Err(io::Error::other("cleanup fence is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let mode = held.permissions().mode() & 0o777;
        if held.dev() != named.dev()
            || held.ino() != named.ino()
            || held.uid() != unsafe { libc::geteuid() }
            || held.nlink() != 1
            || !(mode & !0o600 == 0 || mode == 0o644)
        {
            return Err(io::Error::other(
                "cleanup fence identity or ownership changed",
            ));
        }
        // Creation mode is still filtered by the caller's umask. Repair only
        // an already-proved owner inode whose mode is a subset of 0600 (or the
        // historical 0644 daemon lock), then require the exact private mode.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        held = file.metadata()?;
        named = std::fs::symlink_metadata(path)?;
        if held.dev() != named.dev()
            || held.ino() != named.ino()
            || held.uid() != unsafe { libc::geteuid() }
            || held.nlink() != 1
            || held.permissions().mode() & 0o777 != 0o600
        {
            return Err(io::Error::other(
                "cleanup fence identity or ownership changed",
            ));
        }
    }
    Ok(())
}

fn describe(candidate: &CleanCandidate) -> String {
    match candidate {
        CleanCandidate::Workspace { path } => format!("workspace {}", path.display()),
        CleanCandidate::Data { root, dir } => {
            format!(
                "data {} (missing workspace {})",
                dir.display(),
                root.display()
            )
        }
        CleanCandidate::Worktree {
            path,
            requires_force,
            ..
        } => format!(
            "worktree {}{}",
            path.display(),
            if *requires_force {
                " [force required]"
            } else {
                ""
            }
        ),
        CleanCandidate::Branch {
            name,
            root,
            requires_force,
        } => format!(
            "branch {name} in {}{}",
            root.display(),
            if *requires_force {
                " [force required]"
            } else {
                ""
            }
        ),
        CleanCandidate::Process {
            pid,
            role,
            executable,
            requires_force,
            ..
        } => format!(
            "{} pid {pid} from the vanished build {}{}",
            match role {
                HelperRole::Daemon => "daemon",
                HelperRole::Broker => "bootstrap broker",
            },
            executable.display(),
            if *requires_force {
                " [force required]"
            } else {
                ""
            }
        ),
    }
}

/// How long a reaped helper is given to answer each signal.
const HELPER_REAP_POLL: std::time::Duration = std::time::Duration::from_millis(50);
/// How many polls each signal gets before the next step. A helper owns far less
/// than a daemon generation, so a short window is enough.
const HELPER_REAP_POLLS: usize = 100;

/// Observe the `usagi` helper processes this host is running for this user.
///
/// An enumeration that cannot be taken yields nothing rather than an error: the
/// rest of `clean` is about files, and a host without a usable `ps` must not
/// stop a workspace cleanup.
#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=helper_process_listing_is_read_conservatively
fn discover_processes(daemon_dir: &Path) -> Vec<ObservedProcess> {
    let Ok(listing) = std::process::Command::new("ps")
        .args(["-A", "-o", PS_FORMAT])
        .output()
    else {
        return Vec::new();
    };
    if !listing.status.success() {
        return Vec::new();
    }
    let accounted = accounted_pids(daemon_dir);
    // SAFETY: `geteuid` reads the calling process's own effective uid and has
    // no arguments, no allocation, and no failure mode.
    let uid = unsafe { libc::geteuid() };
    parse_helper_processes(
        &String::from_utf8_lossy(&listing.stdout),
        uid,
        std::process::id(),
    )
    .into_iter()
    .map(|(pid, role, executable)| ObservedProcess {
        executable_exists: path_node_may_exist(&executable),
        start_identity: crate::runtime::daemon::process_start_identity(pid).unwrap_or_default(),
        accounted: accounted.contains(&pid),
        pid,
        role,
        executable,
    })
    .collect()
}

/// Every pid this data home still refers to.
///
/// The lifecycle record, the generation registry, and the broker records all
/// name pids this installation is relying on. Collecting them by field name
/// rather than by document shape keeps the fence intact when those documents
/// grow: a pid this data home mentions anywhere is not residue.
#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=helper_process_listing_is_read_conservatively
fn accounted_pids(daemon_dir: &Path) -> BTreeSet<u32> {
    let mut pids = BTreeSet::new();
    let Ok(entries) = std::fs::read_dir(daemon_dir) else {
        return pids;
    };
    for entry in entries.flatten() {
        if entry.path().extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(document) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        collect_pids(&document, &mut pids);
    }
    pids
}

/// Collect every `"pid"` number reachable in `value`.
fn collect_pids(value: &serde_json::Value, into: &mut BTreeSet<u32>) {
    match value {
        serde_json::Value::Object(fields) => {
            for (key, child) in fields {
                if key == "pid"
                    && let Some(pid) = child.as_u64().and_then(|pid| u32::try_from(pid).ok())
                {
                    into.insert(pid);
                }
                collect_pids(child, into);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_pids(item, into);
            }
        }
        _ => {}
    }
}

/// The `ps` fields this plan reads, in this order.
///
/// `args` is last because it is the only field that can contain spaces.
const PS_FORMAT: &str = "pid=,uid=,args=";

/// Turn one `ps` listing into the helper processes this plan may judge.
///
/// Only the daemon and the broker are recognised, and only when they were
/// spawned the way usagi spawns them: an absolute executable path followed by
/// the exact verb. Anything else — another user's process, this process, a
/// `usagi` invoked from `PATH` as a bare name — is not something this plan can
/// identify well enough to end.
fn parse_helper_processes(
    listing: &str,
    uid: u32,
    own_pid: u32,
) -> Vec<(u32, HelperRole, PathBuf)> {
    listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<u32>().ok()?;
            let owner = fields.next()?.parse::<u32>().ok()?;
            let executable = fields.next()?;
            let role = helper_role(fields)?;
            (owner == uid && pid != own_pid && Path::new(executable).is_absolute())
                .then(|| (pid, role, PathBuf::from(executable)))
        })
        .collect()
}

/// Which helper an argument tail names, if any.
fn helper_role<'a>(argv: impl Iterator<Item = &'a str>) -> Option<HelperRole> {
    let tail = argv.collect::<Vec<_>>();
    match tail.as_slice() {
        ["daemon", "serve"] | ["daemon", "serve", "--standby"] => Some(HelperRole::Daemon),
        ["daemon", "bootstrap-broker"] => Some(HelperRole::Broker),
        _ => None,
    }
}

struct SystemGit;

impl GitRunner for SystemGit {
    #[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=clean_planner_classifies_all_effects
    fn run(&self, repo: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
        let output = confined_git_command(repo).args(args).output()?;
        Ok(GitOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GitOutput, GitRunner, acquire_exclusive_fence, clean_exit_code, collect_pids, describe,
        ensure_data_unlinked, ensure_managed_worktree, parse_helper_processes, remove_daemon_data,
    };
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    use std::process::ExitCode;
    use usagi_core::usecase::clean::{CleanCandidate, HelperRole};

    /// A `ps` listing is the only thing standing between this plan and a signal,
    /// so it is read conservatively: another user's process, this process, a
    /// non-absolute executable, and any verb that is not one usagi spawns are all
    /// outside what the plan can identify well enough to end.
    #[test]
    fn helper_process_listing_is_read_conservatively() {
        let listing = "\
  101 501 /opt/usagi/bin/usagi daemon serve
  102 501 /opt/usagi/bin/usagi daemon bootstrap-broker
  103 501 /opt/usagi/bin/usagi daemon serve --standby
  104 502 /opt/usagi/bin/usagi daemon serve
  105 501 usagi daemon serve
  106 501 /opt/usagi/bin/usagi daemon stop
  107 501 /opt/usagi/bin/usagi mcp
  108 501 /opt/usagi/bin/usagi
  999 501 /opt/usagi/bin/usagi daemon serve
not a process line
";

        assert_eq!(
            parse_helper_processes(listing, 501, 999),
            vec![
                (
                    101,
                    HelperRole::Daemon,
                    PathBuf::from("/opt/usagi/bin/usagi")
                ),
                (
                    102,
                    HelperRole::Broker,
                    PathBuf::from("/opt/usagi/bin/usagi")
                ),
                (
                    103,
                    HelperRole::Daemon,
                    PathBuf::from("/opt/usagi/bin/usagi")
                ),
            ],
            "only this user's daemon and broker, spawned from an absolute path, \
             and never this process itself"
        );
    }

    /// A pid this data home mentions anywhere is one it is relying on. Reading
    /// them by field name rather than by document shape keeps that fence intact
    /// as the documents grow.
    #[test]
    fn every_recorded_pid_is_collected_whatever_document_holds_it() {
        let document = serde_json::json!({
            "pid": 1,
            "handoff": { "process": { "pid": 2 } },
            "generations": [{ "process": { "pid": 3 } }, { "process": { "pid": 4 } }],
            "unrelated": "pid",
            "negative": { "pid": -5 },
            "huge": { "pid": 9_999_999_999_u64 },
        });

        let mut pids = BTreeSet::new();
        collect_pids(&document, &mut pids);
        assert_eq!(pids, BTreeSet::from([1, 2, 3, 4]));
    }

    #[derive(Clone)]
    struct FakeGit(GitOutput);

    impl GitRunner for FakeGit {
        fn run(&self, _repo: &Path, _args: &[&str]) -> anyhow::Result<GitOutput> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn clean_planner_classifies_all_effects() {
        let rendered = [
            CleanCandidate::Workspace {
                path: "/missing".into(),
            },
            CleanCandidate::Data {
                root: "/missing".into(),
                dir: "/data/w/one".into(),
            },
            CleanCandidate::Worktree {
                root: "/repo".into(),
                path: "/repo/.usagi/sessions/x".into(),
                requires_force: true,
            },
            CleanCandidate::Branch {
                root: "/repo".into(),
                name: "usagi/x".into(),
                requires_force: true,
            },
            CleanCandidate::Worktree {
                root: "/repo".into(),
                path: "/repo/.usagi/sessions/safe".into(),
                requires_force: false,
            },
            CleanCandidate::Branch {
                root: "/repo".into(),
                name: "usagi/safe".into(),
                requires_force: false,
            },
            CleanCandidate::Process {
                pid: 4242,
                role: HelperRole::Daemon,
                executable: "/gone/target/debug/usagi".into(),
                start_identity: "identity".into(),
                requires_force: true,
            },
            CleanCandidate::Process {
                pid: 4243,
                role: HelperRole::Broker,
                executable: "/gone/target/debug/usagi".into(),
                start_identity: "identity".into(),
                requires_force: false,
            },
        ]
        .map(|candidate| describe(&candidate));
        assert!(rendered[0].starts_with("workspace "));
        assert!(rendered[1].contains("missing workspace"));
        assert!(rendered[2].contains("force required"));
        assert!(rendered[3].contains("usagi/x"));
        assert_eq!(rendered[4], "worktree /repo/.usagi/sessions/safe");
        assert_eq!(rendered[5], "branch usagi/safe in /repo");
        assert_eq!(
            rendered[6],
            "daemon pid 4242 from the vanished build /gone/target/debug/usagi [force required]"
        );
        assert_eq!(
            rendered[7],
            "bootstrap broker pid 4243 from the vanished build /gone/target/debug/usagi"
        );
    }

    #[test]
    fn protected_candidates_are_not_command_failures() {
        assert_eq!(clean_exit_code(0), ExitCode::SUCCESS);
        assert_eq!(clean_exit_code(1), ExitCode::FAILURE);
    }

    #[test]
    fn managed_worktree_revalidation_refuses_branch_replacement() {
        let path = Path::new("/repo/.usagi/sessions/x");
        let output = |branch: Option<&str>| GitOutput {
            success: true,
            stdout: match branch {
                Some(branch) => format!(
                    "worktree {}\nHEAD abc\nbranch refs/heads/{branch}\n\n",
                    path.display()
                ),
                None => format!("worktree {}\nHEAD abc\ndetached\n\n", path.display()),
            },
            stderr: String::new(),
        };
        for branch in [Some("usagi/x"), None] {
            ensure_managed_worktree(&FakeGit(output(branch)), Path::new("/repo"), path, "x")
                .unwrap();
        }
        let error = ensure_managed_worktree(
            &FakeGit(output(Some("feature/reused"))),
            Path::new("/repo"),
            path,
            "x",
        )
        .unwrap_err();
        assert!(error.to_string().contains("identity changed"));
        ensure_managed_worktree(
            &FakeGit(GitOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            }),
            Path::new("/repo"),
            path,
            "x",
        )
        .unwrap();
        assert!(
            ensure_managed_worktree(
                &FakeGit(GitOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: "broken".into(),
                }),
                Path::new("/repo"),
                path,
                "x",
            )
            .is_err()
        );
    }

    #[test]
    fn daemon_data_cleanup_revalidates_binding_and_containment() {
        let daemon = tempfile::tempdir().unwrap();
        let missing_root = daemon.path().join("missing-workspace");
        let state =
            usagi_core::infrastructure::workspace_state::resolve(daemon.path(), &missing_root)
                .unwrap();
        assert!(ensure_data_unlinked(daemon.path(), &missing_root, state.dir()).is_err());
        usagi_core::infrastructure::store::lifecycle::DaemonLifecycleStore::new(state.dir())
            .initialize(
                &usagi_core::domain::session_lifecycle::WorkspaceLifecycleState::new(
                    usagi_core::domain::id::WorkspaceId::new(),
                    chrono::Utc::now(),
                ),
                &missing_root,
            )
            .unwrap();
        ensure_data_unlinked(daemon.path(), &missing_root, state.dir()).unwrap();
        assert!(ensure_data_unlinked(daemon.path(), Path::new("relative"), state.dir()).is_err());
        assert!(
            ensure_data_unlinked(daemon.path(), &missing_root, &daemon.path().join("other"))
                .is_err()
        );
        std::fs::create_dir(&missing_root).unwrap();
        assert!(ensure_data_unlinked(daemon.path(), &missing_root, state.dir()).is_err());

        let container = daemon.path().join("owned");
        std::fs::create_dir(&container).unwrap();
        let target = container.join("target");
        std::fs::create_dir(&target).unwrap();
        remove_daemon_data(&container, &target).unwrap();
        assert!(!target.exists());
        let outside = daemon.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        assert!(remove_daemon_data(&container, &outside).is_err());
        assert!(remove_daemon_data(&container, &container.join("missing")).is_err());

        #[cfg(unix)]
        {
            let broken_root = daemon.path().join("broken-root");
            std::os::unix::fs::symlink(daemon.path().join("absent"), &broken_root).unwrap();
            assert!(ensure_data_unlinked(daemon.path(), &broken_root, state.dir()).is_err());

            let real = daemon.path().join("real");
            std::fs::create_dir(&real).unwrap();
            let link = container.join("link");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert!(remove_daemon_data(&container, &link).is_err());
            assert!(real.exists());
        }
    }

    #[test]
    fn cleanup_fence_rejects_contention_and_insecure_nodes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("private/daemon.lock");
        let held = acquire_exclusive_fence(&path, "busy").unwrap();
        assert!(acquire_exclusive_fence(&path, "busy").is_err());
        drop(held);
        assert!(acquire_exclusive_fence(&path, "busy").is_ok());

        #[cfg(unix)]
        {
            let target = temp.path().join("target");
            std::fs::write(&target, "keep").unwrap();
            let link = temp.path().join("private/link.lock");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert!(acquire_exclusive_fence(&link, "busy").is_err());
            assert_eq!(std::fs::read_to_string(target).unwrap(), "keep");
        }
    }
}

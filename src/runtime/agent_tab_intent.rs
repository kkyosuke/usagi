//! Secure user-local persistence for TUI Agent tab display intent.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use fs2::FileExt;
use usagi_core::domain::id::WorkspaceId;
use usagi_tui::usecase::application::agent_tab_intent::{
    AGENT_TAB_INTENT_SCHEMA, AgentTabIntent, AgentTabIntentError, AgentTabIntentMutation,
    AgentTabIntentPort, AgentTabIntentPortCommit,
};

static TEMPORARY_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    static FAIL_BEFORE_RENAME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// File-backed Agent tab intent rooted in the selected user data directory.
#[derive(Debug, Clone)]
pub(crate) struct FileAgentTabIntentStore {
    data_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AgentTabIntentLoad {
    Loaded(AgentTabIntent),
    Missing,
    Corrupt,
    FutureSchema(u32),
}

impl FileAgentTabIntentStore {
    /// Creates a store below `<data-dir>/tui/workspaces`.
    pub(crate) const fn new(data_dir: PathBuf) -> Self {
        Self { data_dir }
    }

    pub(crate) fn open_default() -> io::Result<Self> {
        usagi_core::infrastructure::paths::data_dir()
            .map(Self::new)
            .map_err(io::Error::other)
    }

    fn workspace_dir(&self, workspace: WorkspaceId) -> PathBuf {
        self.data_dir
            .join("tui")
            .join("workspaces")
            .join(workspace.as_str())
    }

    fn state_path(&self, workspace: WorkspaceId) -> PathBuf {
        self.workspace_dir(workspace).join("agent-tabs.json")
    }

    fn with_lock<T>(
        &self,
        workspace: WorkspaceId,
        operation: impl FnOnce(&Path) -> io::Result<T>,
    ) -> io::Result<T> {
        let workspace_dir = self.workspace_dir(workspace);
        ensure_private_tree(&self.data_dir)?;
        ensure_private_tree(&self.data_dir.join("tui"))?;
        ensure_private_tree(&self.data_dir.join("tui").join("workspaces"))?;
        ensure_private_tree(&workspace_dir)?;
        let lock_path = workspace_dir.join("agent-tabs.lock");
        let lock = open_private_lock(&lock_path)?;
        FileExt::lock_exclusive(&lock)?;
        operation(&self.state_path(workspace))
    }

    fn read_unlocked(path: &Path, workspace: WorkspaceId) -> io::Result<AgentTabIntentLoad> {
        let Some(contents) = read_private_file(path)? else {
            return Ok(AgentTabIntentLoad::Missing);
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents) else {
            quarantine(path)?;
            return Ok(AgentTabIntentLoad::Corrupt);
        };
        let Some(version) = value
            .get("schema")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
        else {
            quarantine(path)?;
            return Ok(AgentTabIntentLoad::Corrupt);
        };
        if version > AGENT_TAB_INTENT_SCHEMA {
            return Ok(AgentTabIntentLoad::FutureSchema(version));
        }
        if version != AGENT_TAB_INTENT_SCHEMA {
            quarantine(path)?;
            return Ok(AgentTabIntentLoad::Corrupt);
        }
        let Ok(intent) = serde_json::from_value::<AgentTabIntent>(value) else {
            quarantine(path)?;
            return Ok(AgentTabIntentLoad::Corrupt);
        };
        if intent.validate(workspace).is_err() {
            quarantine(path)?;
            return Ok(AgentTabIntentLoad::Corrupt);
        }
        Ok(AgentTabIntentLoad::Loaded(intent))
    }

    #[cfg(test)]
    fn load_status(&self, workspace: WorkspaceId) -> io::Result<AgentTabIntentLoad> {
        self.with_lock(workspace, |path| Self::read_unlocked(path, workspace))
    }

    fn write_unlocked(path: &Path, intent: &AgentTabIntent) -> io::Result<()> {
        // Validate any extant path before rename so a symlink/hardlink attack is
        // rejected rather than silently replaced.
        let _ = read_private_file(path)?;
        let mut contents = serde_json::to_vec_pretty(intent).map_err(io::Error::other)?;
        contents.push(b'\n');
        let temporary = unique_peer(path, "tmp");
        let mut file = create_private_new(&temporary)?;
        let result = (|| {
            file.write_all(&contents)?;
            file.sync_all()?;
            drop(file);
            #[cfg(test)]
            if FAIL_BEFORE_RENAME.with(std::cell::Cell::take) {
                return Err(io::Error::other(
                    "injected Agent tab intent failure before rename",
                ));
            }
            fs::rename(&temporary, path)?;
            sync_parent_best_effort(path);
            Ok(())
        })();
        if let Err(error) = result {
            return rollback_publish(error, fs::remove_file(&temporary));
        }
        Ok(())
    }
}

fn rollback_publish(error: io::Error, cleanup: io::Result<()>) -> io::Result<()> {
    match cleanup {
        Ok(()) => Err(error),
        Err(cleanup) if cleanup.kind() == io::ErrorKind::NotFound => Err(error),
        Err(cleanup) => Err(io::Error::new(
            cleanup.kind(),
            format!("{error}; temporary rollback failed: {cleanup}"),
        )),
    }
}

impl AgentTabIntentPort for FileAgentTabIntentStore {
    fn load(&mut self, workspace: WorkspaceId) -> Result<AgentTabIntent, AgentTabIntentError> {
        self.with_lock(workspace, |path| Self::read_unlocked(path, workspace))
            .map_err(|_| AgentTabIntentError::Unavailable)
            .and_then(|loaded| match loaded {
                AgentTabIntentLoad::Loaded(intent) => Ok(intent),
                AgentTabIntentLoad::Missing | AgentTabIntentLoad::Corrupt => {
                    Ok(AgentTabIntent::empty(workspace))
                }
                AgentTabIntentLoad::FutureSchema(_) => Err(AgentTabIntentError::ReadOnlySchema),
            })
    }

    #[allow(clippy::too_many_lines)] // Keep the CAS decision and atomic publish in one lock scope.
    fn mutate(
        &mut self,
        workspace: WorkspaceId,
        expected_revision: u64,
        mutation: AgentTabIntentMutation,
    ) -> Result<AgentTabIntentPortCommit, AgentTabIntentError> {
        self.with_lock(workspace, |path| {
            let mut current = match Self::read_unlocked(path, workspace)? {
                AgentTabIntentLoad::Loaded(intent) => intent,
                AgentTabIntentLoad::Missing | AgentTabIntentLoad::Corrupt => {
                    AgentTabIntent::empty(workspace)
                }
                AgentTabIntentLoad::FutureSchema(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "future Agent tab intent schema is read-only",
                    ));
                }
            };
            let cas_conflict = current.revision != expected_revision;
            if expected_revision > current.revision {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Agent tab intent revision is ahead of durable state",
                ));
            }
            let before = current.clone();
            // An accepted close is a causal write even when this key is
            // already dismissed. Otherwise a Reopen that loaded the current
            // revision before this close could clear the newer user intent.
            // Unknown/authoritatively removed keys remain inert.
            let force_close_fence = match &mutation {
                AgentTabIntentMutation::Dismiss { continuation }
                | AgentTabIntentMutation::DismissAndSelect { continuation, .. } => {
                    current.targets.iter().any(|target| {
                        target
                            .tabs
                            .iter()
                            .any(|slot| slot.continuation == *continuation)
                    })
                }
                _ => false,
            };
            let mut mutation_applied = true;
            let projection = if cas_conflict {
                match mutation {
                    AgentTabIntentMutation::Observe {
                        terminals,
                        agents,
                        allowed_sessions,
                    } => {
                        // Observe is not a stable-key delta. Return only a
                        // latest-ref-exact projection, leave bytes untouched,
                        // and make the controller redispatch under a fresh CAS
                        // fence before it changes runtime state.
                        mutation_applied = false;
                        Some(current.projected_exact(&terminals, &agents, &allowed_sessions))
                    }
                    AgentTabIntentMutation::Reopen { continuation } => {
                        // Reopen is anti-monotonic with Dismiss. If this stale
                        // writer still sees the key closed, it cannot distinguish
                        // the dismissal it read from a newer concurrent close;
                        // preserve the latest close and ask the user to retry.
                        mutation_applied = !current.dismissed.contains(&continuation);
                        None
                    }
                    AgentTabIntentMutation::Upsert {
                        session_id,
                        continuation,
                        terminal,
                        select,
                    } => {
                        // A continuation/selection is a same-key register, not
                        // a commutative delta. A stale admission may only be
                        // acknowledged when the latest state already contains
                        // the exact requested value. Otherwise a fresh daemon
                        // observation must decide whether O or R is current.
                        let existing = current.targets.iter().find_map(|target| {
                            target
                                .tabs
                                .iter()
                                .find(|slot| slot.continuation == continuation)
                                .map(|slot| (target, slot))
                        });
                        let already_applied = existing.is_some_and(|(target, slot)| {
                            target.session_id == session_id
                                && slot.terminal.fences(&terminal)
                                && (!select || target.selected == Some(continuation))
                                && !current.dismissed.contains(&continuation)
                        });
                        mutation_applied = already_applied;
                        None
                    }
                    AgentTabIntentMutation::DismissAndSelect { continuation, .. } => {
                        // The close itself is monotonic and safe to merge. Its
                        // local successor preview is stale, so preserve the
                        // latest writer's selection and only merge Dismiss.
                        current.apply(AgentTabIntentMutation::Dismiss { continuation })
                    }
                    AgentTabIntentMutation::Select {
                        session_id,
                        continuation,
                    } => {
                        mutation_applied = current.targets.iter().any(|target| {
                            target.session_id == session_id && target.selected == continuation
                        });
                        None
                    }
                    AgentTabIntentMutation::Reorder {
                        session_id,
                        continuations,
                    } => {
                        mutation_applied = current
                            .targets
                            .iter()
                            .find(|target| target.session_id == session_id)
                            .is_some_and(|target| {
                                target
                                    .tabs
                                    .iter()
                                    .map(|slot| slot.continuation)
                                    .eq(continuations)
                            });
                        None
                    }
                    AgentTabIntentMutation::Dismiss { continuation } => {
                        current.apply(AgentTabIntentMutation::Dismiss { continuation })
                    }
                }
            } else {
                match mutation {
                    AgentTabIntentMutation::Upsert {
                        session_id,
                        continuation,
                        terminal,
                        select: _,
                    } if current.dismissed.contains(&continuation) => {
                        // Upsert refreshes identity, but only Reopen is allowed
                        // to make an explicitly closed lineage visible again.
                        mutation_applied = false;
                        current.apply(AgentTabIntentMutation::Upsert {
                            session_id,
                            continuation,
                            terminal,
                            select: false,
                        })
                    }
                    mutation => current.apply(mutation),
                }
            };
            if current != before || force_close_fence {
                current.revision = current
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("Agent tab intent revision exhausted"))?;
                current.validate(workspace).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid Agent tab intent mutation",
                    )
                })?;
                Self::write_unlocked(path, &current)?;
            }
            Ok(AgentTabIntentPortCommit {
                intent: current,
                projection,
                mutation_applied,
                cas_conflict,
            })
        })
        .map_err(|error| match error.kind() {
            io::ErrorKind::Unsupported => AgentTabIntentError::ReadOnlySchema,
            io::ErrorKind::InvalidData => AgentTabIntentError::InvalidMutation,
            _ => AgentTabIntentError::Unavailable,
        })
    }
}

fn ensure_private_tree(path: &Path) -> io::Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        return make_private_directory(path, &metadata);
    }
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "private directory has no parent",
        )
    })?;
    if !parent.exists() {
        ensure_private_tree(parent)?;
    }
    fs::create_dir(path)?;
    let metadata = fs::symlink_metadata(path)?;
    make_private_directory(path, &metadata)
}

fn make_private_directory(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    validate_private_directory_identity(metadata)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    validate_private_directory_mode(&fs::symlink_metadata(path)?)
}

fn validate_private_directory_identity(metadata: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    if !metadata.file_type().is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Agent tab intent directory is not owner-controlled",
        ));
    }
    Ok(())
}

fn validate_private_directory_mode(metadata: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    if metadata.mode() & 0o777 == 0o700 {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "Agent tab intent directory is not private",
    ))
}

fn open_private_lock(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?;
    verify_private_file(&file)?;
    Ok(file)
}

fn create_private_new(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?;
    verify_private_file(&file)?;
    Ok(file)
}

fn verify_private_file(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    validate_private_file_identity(&file.metadata()?)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    validate_private_file_mode(&file.metadata()?)
}

fn validate_private_file_identity(metadata: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Agent tab intent file is not a private owner file",
        ));
    }
    Ok(())
}

fn validate_private_file_mode(metadata: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    if metadata.mode() & 0o777 == 0o600 {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "Agent tab intent file mode is not private",
    ))
}

fn read_private_file(path: &Path) -> io::Result<Option<String>> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    verify_private_file(&file)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    Ok(Some(contents))
}

fn unique_peer(path: &Path, suffix: &str) -> PathBuf {
    let mut peer = path.as_os_str().to_owned();
    peer.push(format!(
        ".{suffix}.{}.{}",
        std::process::id(),
        TEMPORARY_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    PathBuf::from(peer)
}

fn quarantine(path: &Path) -> io::Result<()> {
    let quarantined = unique_peer(path, "corrupt");
    fs::rename(path, &quarantined)?;
    sync_parent_best_effort(path);
    Ok(())
}

fn sync_parent_best_effort(path: &Path) {
    if let Some(parent) = path.parent()
        && let Ok(directory) = File::open(parent)
    {
        let _ = directory.sync_all();
    }
}

#[cfg(test)]
mod tests {
    #![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::sync::{Arc, Barrier};

    use tempfile::TempDir;
    use usagi_core::domain::agent::{
        AgentInventory, AgentRuntimeInventoryItem, AgentRuntimeInventoryState,
    };
    use usagi_core::domain::id::{
        AgentContinuationRef, AgentRuntimeId, AgentRuntimeRef, DaemonGeneration, SessionId,
        TerminalId, TerminalRef, WorktreeId,
    };
    use usagi_core::domain::terminal_launch::{TerminalInventoryEntry, TerminalKind};
    use usagi_tui::usecase::application::agent_tab_intent::AgentTabIntentMutation;

    use super::*;

    fn fixture() -> (TempDir, FileAgentTabIntentStore, WorkspaceId) {
        let root = tempfile::tempdir().unwrap();
        let store = FileAgentTabIntentStore::new(root.path().join("data"));
        (root, store, WorkspaceId::new())
    }

    #[derive(Clone)]
    struct Observation {
        continuation: AgentContinuationRef,
        terminal: TerminalRef,
    }

    fn observation(workspace: WorkspaceId) -> Observation {
        Observation {
            continuation: AgentContinuationRef::new(),
            terminal: TerminalRef {
                daemon_generation: DaemonGeneration::new(),
                terminal_id: TerminalId::new(),
                workspace_id: workspace,
                session_id: None,
                worktree_id: WorktreeId::new(),
            },
        }
    }

    #[test]
    fn parent_sync_is_best_effort_when_the_parent_is_unavailable() {
        let root = tempfile::tempdir().unwrap();
        sync_parent_best_effort(&root.path().join("missing").join("agent-tabs.json"));
        sync_parent_best_effort(Path::new("/"));
    }

    #[test]
    fn rollback_error_mapping_preserves_publish_or_reports_cleanup_failure() {
        let missing = rollback_publish(
            io::Error::new(io::ErrorKind::InvalidData, "publish failed"),
            Err(io::Error::new(io::ErrorKind::NotFound, "already absent")),
        )
        .unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::InvalidData);
        assert_eq!(missing.to_string(), "publish failed");

        let failed = rollback_publish(
            io::Error::new(io::ErrorKind::InvalidData, "publish failed"),
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cleanup denied",
            )),
        )
        .unwrap_err();
        assert_eq!(failed.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            failed.to_string(),
            "publish failed; temporary rollback failed: cleanup denied"
        );
    }

    #[test]
    fn private_tree_and_metadata_validation_reject_unsafe_shapes_and_modes() {
        let relative = PathBuf::from(format!("missing-private-tree-{}", WorkspaceId::new()));
        assert_eq!(
            ensure_private_tree(&relative).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(!relative.exists());

        let root = tempfile::tempdir().unwrap();
        let regular = root.path().join("regular");
        fs::write(&regular, "not a directory").unwrap();
        assert_eq!(
            validate_private_directory_identity(&fs::symlink_metadata(&regular).unwrap())
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );

        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            validate_private_directory_mode(&fs::symlink_metadata(root.path()).unwrap())
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            validate_private_file_identity(&fs::symlink_metadata(root.path()).unwrap())
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );

        fs::set_permissions(&regular, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            validate_private_file_mode(&fs::symlink_metadata(&regular).unwrap())
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn missing_state_commits_private_atomic_file_and_tightens_modes() {
        let (_root, mut store, workspace) = fixture();
        assert_eq!(
            store.load_status(workspace).unwrap(),
            AgentTabIntentLoad::Missing
        );
        let observation = observation(workspace);
        let committed = store
            .mutate(
                workspace,
                0,
                AgentTabIntentMutation::Upsert {
                    session_id: None,
                    continuation: observation.continuation,
                    terminal: observation.terminal,
                    select: true,
                },
            )
            .unwrap();
        assert_eq!(committed.intent.revision, 1);
        assert_eq!(
            AgentTabIntentPort::load(&mut store, workspace).unwrap(),
            committed.intent
        );
        let state = store.state_path(workspace);
        assert_eq!(state.metadata().unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            store.workspace_dir(workspace).metadata().unwrap().mode() & 0o777,
            0o700
        );
        fs::set_permissions(&state, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            store.load_status(workspace).unwrap(),
            AgentTabIntentLoad::Loaded(_)
        ));
        assert_eq!(state.metadata().unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn symlink_and_hardlink_state_and_lock_are_rejected() {
        let (root, mut store, workspace) = fixture();
        let _ = store.load(workspace).unwrap();
        let state = store.state_path(workspace);
        let victim = root.path().join("victim");
        fs::write(&victim, "victim").unwrap();
        symlink(&victim, &state).unwrap();
        assert!(store.load(workspace).is_err());
        fs::remove_file(&state).unwrap();
        fs::hard_link(&victim, &state).unwrap();
        assert!(store.load(workspace).is_err());
        fs::remove_file(&state).unwrap();

        let lock = store.workspace_dir(workspace).join("agent-tabs.lock");
        fs::remove_file(&lock).unwrap();
        symlink(&victim, &lock).unwrap();
        assert!(store.load(workspace).is_err());
        fs::remove_file(&lock).unwrap();
        fs::hard_link(&victim, &lock).unwrap();
        assert!(store.load(workspace).is_err());
        assert_eq!(victim.metadata().unwrap().nlink(), 2);
        assert_eq!(fs::read_to_string(victim).unwrap(), "victim");
    }

    #[test]
    fn interrupted_publish_preserves_old_valid_state() {
        let (_root, mut store, workspace) = fixture();
        let first = observation(workspace);
        let old = store
            .mutate(
                workspace,
                0,
                AgentTabIntentMutation::Upsert {
                    session_id: None,
                    continuation: first.continuation,
                    terminal: first.terminal,
                    select: true,
                },
            )
            .unwrap()
            .intent;
        FAIL_BEFORE_RENAME.with(|fail| fail.set(true));
        let second = observation(workspace);
        assert!(
            store
                .mutate(
                    workspace,
                    old.revision,
                    AgentTabIntentMutation::Upsert {
                        session_id: None,
                        continuation: second.continuation,
                        terminal: second.terminal,
                        select: false,
                    },
                )
                .is_err()
        );
        assert_eq!(
            store.load_status(workspace).unwrap(),
            AgentTabIntentLoad::Loaded(old)
        );
    }

    #[test]
    fn future_schema_is_preserved_in_place_and_never_overwritten() {
        let (_root, mut store, workspace) = fixture();
        let _ = store.load(workspace).unwrap();
        let path = store.state_path(workspace);
        let future = format!(
            "{{\"schema\":{},\"sentinel\":\"keep\"}}",
            AGENT_TAB_INTENT_SCHEMA + 1
        );
        fs::write(&path, &future).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            store.load_status(workspace).unwrap(),
            AgentTabIntentLoad::FutureSchema(AGENT_TAB_INTENT_SCHEMA + 1)
        );
        assert_eq!(
            store.load(workspace),
            Err(AgentTabIntentError::ReadOnlySchema)
        );
        let observation = observation(workspace);
        assert_eq!(
            store.mutate(
                workspace,
                0,
                AgentTabIntentMutation::Upsert {
                    session_id: None,
                    continuation: observation.continuation,
                    terminal: observation.terminal,
                    select: true,
                },
            ),
            Err(AgentTabIntentError::ReadOnlySchema)
        );
        assert_eq!(fs::read_to_string(path).unwrap(), future);
    }

    #[test]
    fn corrupt_state_is_quarantined_without_blocking_empty_fallback() {
        let (_root, mut store, workspace) = fixture();
        let _ = store.load(workspace).unwrap();
        let path = store.state_path(workspace);
        fs::write(&path, "not json").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            store.load_status(workspace).unwrap(),
            AgentTabIntentLoad::Corrupt
        );
        assert!(!path.exists());
        let names = fs::read_dir(store.workspace_dir(workspace))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(names.iter().any(|name| name.contains(".corrupt.")));
    }

    #[test]
    fn malformed_schema_shapes_and_wrong_workspace_are_quarantined() {
        let (_root, mut store, workspace) = fixture();
        let _ = store.load(workspace).unwrap();
        let wrong_workspace =
            serde_json::to_string(&AgentTabIntent::empty(WorkspaceId::new())).unwrap();
        for contents in [
            r#"{"schema":"one"}"#.to_owned(),
            r#"{"schema":0}"#.to_owned(),
            format!(r#"{{"schema":{AGENT_TAB_INTENT_SCHEMA}}}"#),
            wrong_workspace,
        ] {
            let path = store.state_path(workspace);
            fs::write(&path, contents).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(
                store.load_status(workspace).unwrap(),
                AgentTabIntentLoad::Corrupt
            );
            assert!(!path.exists());
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One durable fixture covers every CAS/error-only mutation arm.
    fn mutation_cas_and_validation_failures_preserve_their_causal_contract() {
        let (_root, mut store, workspace) = fixture();
        let first = observation(workspace);
        assert_eq!(
            store.mutate(
                workspace,
                1,
                AgentTabIntentMutation::Dismiss {
                    continuation: first.continuation,
                },
            ),
            Err(AgentTabIntentError::InvalidMutation)
        );

        let first_commit = store
            .mutate(
                workspace,
                0,
                AgentTabIntentMutation::Upsert {
                    session_id: None,
                    continuation: first.continuation,
                    terminal: first.terminal.clone(),
                    select: true,
                },
            )
            .unwrap();
        let second = observation(workspace);
        let second_commit = store
            .mutate(
                workspace,
                first_commit.intent.revision,
                AgentTabIntentMutation::Upsert {
                    session_id: None,
                    continuation: second.continuation,
                    terminal: second.terminal.clone(),
                    select: false,
                },
            )
            .unwrap();

        let stale_exact = store
            .mutate(
                workspace,
                first_commit.intent.revision,
                AgentTabIntentMutation::Upsert {
                    session_id: None,
                    continuation: first.continuation,
                    terminal: first.terminal.clone(),
                    select: true,
                },
            )
            .unwrap();
        assert!(stale_exact.cas_conflict);
        assert!(stale_exact.mutation_applied);
        assert_eq!(stale_exact.intent.revision, second_commit.intent.revision);

        let selected_second = store
            .mutate(
                workspace,
                second_commit.intent.revision,
                AgentTabIntentMutation::Select {
                    session_id: None,
                    continuation: Some(second.continuation),
                },
            )
            .unwrap();
        let stale_close = store
            .mutate(
                workspace,
                second_commit.intent.revision,
                AgentTabIntentMutation::DismissAndSelect {
                    continuation: first.continuation,
                    session_id: None,
                    selected: Some(first.continuation),
                },
            )
            .unwrap();
        assert!(stale_close.cas_conflict);
        assert!(stale_close.intent.dismissed.contains(&first.continuation));
        assert_eq!(
            stale_close.intent.targets[0].selected,
            Some(second.continuation)
        );

        let stale_dismiss = store
            .mutate(
                workspace,
                selected_second.intent.revision,
                AgentTabIntentMutation::Dismiss {
                    continuation: second.continuation,
                },
            )
            .unwrap();
        assert!(stale_dismiss.cas_conflict);
        assert!(
            stale_dismiss
                .intent
                .dismissed
                .contains(&second.continuation)
        );

        let replacement = observation(workspace).terminal;
        let refreshed = store
            .mutate(
                workspace,
                stale_dismiss.intent.revision,
                AgentTabIntentMutation::Upsert {
                    session_id: None,
                    continuation: first.continuation,
                    terminal: replacement.clone(),
                    select: true,
                },
            )
            .unwrap();
        assert!(!refreshed.mutation_applied);
        assert!(refreshed.intent.dismissed.contains(&first.continuation));
        assert!(
            refreshed.intent.targets[0].tabs[0]
                .terminal
                .fences(&replacement)
        );

        let invalid_scope = observation(workspace);
        let session = SessionId::new();
        let mut mismatched_terminal = invalid_scope.terminal;
        mismatched_terminal.session_id = Some(session);
        let before = fs::read(store.state_path(workspace)).unwrap();
        assert_eq!(
            store.mutate(
                workspace,
                refreshed.intent.revision,
                AgentTabIntentMutation::Upsert {
                    session_id: None,
                    continuation: invalid_scope.continuation,
                    terminal: mismatched_terminal,
                    select: false,
                },
            ),
            Err(AgentTabIntentError::InvalidMutation)
        );
        assert_eq!(fs::read(store.state_path(workspace)).unwrap(), before);
    }

    #[test]
    fn exhausted_revision_is_rejected_without_changing_durable_bytes() {
        let (_root, mut store, workspace) = fixture();
        let _ = store.load(workspace).unwrap();
        let observed = observation(workspace);
        let mut intent = AgentTabIntent::empty(workspace);
        intent.apply(AgentTabIntentMutation::Upsert {
            session_id: None,
            continuation: observed.continuation,
            terminal: observed.terminal,
            select: true,
        });
        intent.revision = u64::MAX;
        let path = store.state_path(workspace);
        let bytes = serde_json::to_vec_pretty(&intent).unwrap();
        fs::write(&path, &bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        assert_eq!(
            store.mutate(
                workspace,
                u64::MAX,
                AgentTabIntentMutation::Select {
                    session_id: None,
                    continuation: None,
                },
            ),
            Err(AgentTabIntentError::Unavailable)
        );
        assert_eq!(fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn exhausted_revision_rejects_an_idempotent_known_close_without_changing_bytes() {
        let (_root, mut store, workspace) = fixture();
        let _ = store.load(workspace).unwrap();
        let observed = observation(workspace);
        let mut intent = AgentTabIntent::empty(workspace);
        intent.apply(AgentTabIntentMutation::Upsert {
            session_id: None,
            continuation: observed.continuation,
            terminal: observed.terminal,
            select: true,
        });
        intent.apply(AgentTabIntentMutation::Dismiss {
            continuation: observed.continuation,
        });
        intent.revision = u64::MAX;
        let path = store.state_path(workspace);
        let bytes = serde_json::to_vec_pretty(&intent).unwrap();
        fs::write(&path, &bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        assert_eq!(
            store.mutate(
                workspace,
                u64::MAX,
                AgentTabIntentMutation::Dismiss {
                    continuation: observed.continuation,
                },
            ),
            Err(AgentTabIntentError::Unavailable)
        );
        assert_eq!(fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn stale_reorder_never_overwrites_a_concurrent_monotonic_dismiss() {
        let (_root, mut setup, workspace) = fixture();
        let first = observation(workspace);
        let second = observation(workspace);
        let first_commit = setup
            .mutate(
                workspace,
                0,
                AgentTabIntentMutation::Upsert {
                    session_id: None,
                    continuation: first.continuation,
                    terminal: first.terminal.clone(),
                    select: true,
                },
            )
            .unwrap();
        let initial = setup
            .mutate(
                workspace,
                first_commit.intent.revision,
                AgentTabIntentMutation::Upsert {
                    session_id: None,
                    continuation: second.continuation,
                    terminal: second.terminal.clone(),
                    select: false,
                },
            )
            .unwrap()
            .intent;
        let barrier = Arc::new(Barrier::new(2));
        let mut dismiss_store = setup.clone();
        let mut move_store = setup.clone();
        let dismiss_barrier = Arc::clone(&barrier);
        let dismiss = std::thread::spawn(move || {
            dismiss_barrier.wait();
            dismiss_store
                .mutate(
                    workspace,
                    initial.revision,
                    AgentTabIntentMutation::Dismiss {
                        continuation: first.continuation,
                    },
                )
                .unwrap()
        });
        let move_barrier = Arc::clone(&barrier);
        let moved = std::thread::spawn(move || {
            move_barrier.wait();
            move_store
                .mutate(
                    workspace,
                    initial.revision,
                    AgentTabIntentMutation::Reorder {
                        session_id: None,
                        continuations: vec![second.continuation, first.continuation],
                    },
                )
                .unwrap()
        });
        let dismiss = dismiss.join().unwrap();
        let moved = moved.join().unwrap();
        assert_ne!(dismiss.cas_conflict, moved.cas_conflict);
        let AgentTabIntentLoad::Loaded(final_state) = setup.load_status(workspace).unwrap() else {
            panic!("expected merged state");
        };
        assert_eq!(
            final_state.revision,
            initial.revision + 1 + u64::from(moved.mutation_applied)
        );
        assert_eq!(final_state.dismissed, [first.continuation].into());
        let expected_first = if moved.mutation_applied {
            second.continuation
        } else {
            first.continuation
        };
        assert_eq!(final_state.targets[0].tabs[0].continuation, expected_first);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Stale and fresh observations share one exact-ref fixture.
    fn stale_observe_projects_without_replacing_newer_durable_terminal_ref() {
        let (_root, mut store, workspace) = fixture();
        let old = observation(workspace);
        let first = store
            .mutate(
                workspace,
                0,
                AgentTabIntentMutation::Upsert {
                    session_id: None,
                    continuation: old.continuation,
                    terminal: old.terminal.clone(),
                    select: true,
                },
            )
            .unwrap();
        let replacement = observation(workspace);
        let latest = store
            .mutate(
                workspace,
                first.intent.revision,
                AgentTabIntentMutation::Upsert {
                    session_id: None,
                    continuation: old.continuation,
                    terminal: replacement.terminal.clone(),
                    select: true,
                },
            )
            .unwrap();
        let path = store.state_path(workspace);
        let bytes_before = fs::read(&path).unwrap();
        let runtime =
            AgentRuntimeRef::new(AgentRuntimeId::new(), old.terminal.clone(), None).unwrap();

        let stale = store
            .mutate(
                workspace,
                first.intent.revision,
                AgentTabIntentMutation::Observe {
                    terminals: vec![TerminalInventoryEntry {
                        terminal: old.terminal.clone(),
                        kind: TerminalKind::Agent,
                        live: true,
                    }],
                    agents: AgentInventory {
                        workspace_id: workspace,
                        runtimes: vec![AgentRuntimeInventoryItem {
                            runtime,
                            continuation: old.continuation,
                            state: AgentRuntimeInventoryState::Live,
                            resumed_from: None,
                        }],
                        resumable: Vec::new(),
                    },
                    allowed_sessions: std::collections::BTreeSet::default(),
                },
            )
            .unwrap();

        assert!(stale.cas_conflict);
        assert!(!stale.mutation_applied);
        assert_eq!(stale.intent.revision, latest.intent.revision);
        assert_eq!(
            stale.intent.targets[0].tabs[0].terminal,
            replacement.terminal
        );
        let stale_projection = stale.projection.expect("stale Observe projects safely");
        assert!(stale_projection.targets.iter().all(|target| {
            target
                .tabs
                .iter()
                .all(|slot| !slot.terminal.fences(&old.terminal))
        }));
        assert!(stale_projection.targets.is_empty());
        assert_eq!(fs::read(&path).unwrap(), bytes_before);

        let replacement_runtime =
            AgentRuntimeRef::new(AgentRuntimeId::new(), replacement.terminal.clone(), None)
                .unwrap();
        let fresh = store
            .mutate(
                workspace,
                latest.intent.revision,
                AgentTabIntentMutation::Observe {
                    terminals: vec![TerminalInventoryEntry {
                        terminal: replacement.terminal.clone(),
                        kind: TerminalKind::Agent,
                        live: true,
                    }],
                    agents: AgentInventory {
                        workspace_id: workspace,
                        runtimes: vec![AgentRuntimeInventoryItem {
                            runtime: replacement_runtime,
                            continuation: old.continuation,
                            state: AgentRuntimeInventoryState::Live,
                            resumed_from: None,
                        }],
                        resumable: Vec::new(),
                    },
                    allowed_sessions: std::collections::BTreeSet::default(),
                },
            )
            .unwrap();
        assert!(fresh.mutation_applied);
        assert!(!fresh.cas_conflict);
        assert_eq!(
            fresh.projection.unwrap().targets[0].tabs[0].terminal,
            replacement.terminal
        );
    }

    #[test]
    fn stale_upsert_cannot_replace_a_newer_exact_terminal_register() {
        let (_root, mut store, workspace) = fixture();
        let old = observation(workspace);
        let first = store
            .mutate(
                workspace,
                0,
                AgentTabIntentMutation::Upsert {
                    session_id: None,
                    continuation: old.continuation,
                    terminal: old.terminal.clone(),
                    select: true,
                },
            )
            .unwrap();
        let replacement = observation(workspace).terminal;
        let latest = store
            .mutate(
                workspace,
                first.intent.revision,
                AgentTabIntentMutation::Upsert {
                    session_id: None,
                    continuation: old.continuation,
                    terminal: replacement.clone(),
                    select: true,
                },
            )
            .unwrap();
        let path = store.state_path(workspace);
        let bytes_before = fs::read(&path).unwrap();

        let stale = store
            .mutate(
                workspace,
                first.intent.revision,
                AgentTabIntentMutation::Upsert {
                    session_id: None,
                    continuation: old.continuation,
                    terminal: old.terminal,
                    select: true,
                },
            )
            .unwrap();

        assert!(stale.cas_conflict);
        assert!(!stale.mutation_applied);
        assert_eq!(stale.intent.revision, latest.intent.revision);
        assert!(
            stale.intent.targets[0].tabs[0]
                .terminal
                .fences(&replacement)
        );
        assert_eq!(fs::read(path).unwrap(), bytes_before);
    }

    #[test]
    fn stale_select_and_reorder_cannot_overwrite_newer_same_target_registers() {
        let (_root, mut store, workspace) = fixture();
        let first = observation(workspace);
        let second = observation(workspace);
        let first_commit = store
            .mutate(
                workspace,
                0,
                AgentTabIntentMutation::Upsert {
                    session_id: None,
                    continuation: first.continuation,
                    terminal: first.terminal,
                    select: true,
                },
            )
            .unwrap();
        let base = store
            .mutate(
                workspace,
                first_commit.intent.revision,
                AgentTabIntentMutation::Upsert {
                    session_id: None,
                    continuation: second.continuation,
                    terminal: second.terminal,
                    select: false,
                },
            )
            .unwrap();
        let selected = store
            .mutate(
                workspace,
                base.intent.revision,
                AgentTabIntentMutation::Select {
                    session_id: None,
                    continuation: Some(second.continuation),
                },
            )
            .unwrap();
        let selected_bytes = fs::read(store.state_path(workspace)).unwrap();
        let stale_select = store
            .mutate(
                workspace,
                base.intent.revision,
                AgentTabIntentMutation::Select {
                    session_id: None,
                    continuation: Some(first.continuation),
                },
            )
            .unwrap();
        assert!(!stale_select.mutation_applied);
        assert_eq!(
            stale_select.intent.targets[0].selected,
            Some(second.continuation)
        );
        assert_eq!(
            fs::read(store.state_path(workspace)).unwrap(),
            selected_bytes
        );

        let reordered = store
            .mutate(
                workspace,
                selected.intent.revision,
                AgentTabIntentMutation::Reorder {
                    session_id: None,
                    continuations: vec![second.continuation, first.continuation],
                },
            )
            .unwrap();
        let reordered_bytes = fs::read(store.state_path(workspace)).unwrap();
        let stale_reorder = store
            .mutate(
                workspace,
                selected.intent.revision,
                AgentTabIntentMutation::Reorder {
                    session_id: None,
                    continuations: vec![first.continuation, second.continuation],
                },
            )
            .unwrap();
        assert!(!stale_reorder.mutation_applied);
        assert_eq!(stale_reorder.intent.revision, reordered.intent.revision);
        assert_eq!(
            stale_reorder.intent.targets[0]
                .tabs
                .iter()
                .map(|slot| slot.continuation)
                .collect::<Vec<_>>(),
            [second.continuation, first.continuation]
        );
        assert_eq!(
            fs::read(store.state_path(workspace)).unwrap(),
            reordered_bytes
        );
    }

    #[test]
    fn stale_reopen_cannot_clear_a_newer_dismiss_for_the_same_continuation() {
        let (_root, mut store, workspace) = fixture();
        let observed = observation(workspace);
        let upserted = store
            .mutate(
                workspace,
                0,
                AgentTabIntentMutation::Upsert {
                    session_id: None,
                    continuation: observed.continuation,
                    terminal: observed.terminal,
                    select: true,
                },
            )
            .unwrap();
        let initially_closed = store
            .mutate(
                workspace,
                upserted.intent.revision,
                AgentTabIntentMutation::Dismiss {
                    continuation: observed.continuation,
                },
            )
            .unwrap();
        let stale_revision = initially_closed.intent.revision;

        let concurrently_reopened = store
            .mutate(
                workspace,
                stale_revision,
                AgentTabIntentMutation::Reopen {
                    continuation: observed.continuation,
                },
            )
            .unwrap();
        let newer_close = store
            .mutate(
                workspace,
                concurrently_reopened.intent.revision,
                AgentTabIntentMutation::Dismiss {
                    continuation: observed.continuation,
                },
            )
            .unwrap();
        let path = store.state_path(workspace);
        let bytes_before = fs::read(&path).unwrap();

        let stale_reopen = store
            .mutate(
                workspace,
                stale_revision,
                AgentTabIntentMutation::Reopen {
                    continuation: observed.continuation,
                },
            )
            .unwrap();

        assert!(stale_reopen.cas_conflict);
        assert!(!stale_reopen.mutation_applied);
        assert_eq!(stale_reopen.intent.revision, newer_close.intent.revision);
        assert!(
            stale_reopen
                .intent
                .dismissed
                .contains(&observed.continuation)
        );
        assert_eq!(fs::read(path).unwrap(), bytes_before);
    }

    #[test]
    fn idempotent_close_advances_its_fence_before_an_older_reopen() {
        let (_root, mut store, workspace) = fixture();
        let observed = observation(workspace);
        let opened = store
            .mutate(
                workspace,
                0,
                AgentTabIntentMutation::Upsert {
                    session_id: None,
                    continuation: observed.continuation,
                    terminal: observed.terminal,
                    select: true,
                },
            )
            .unwrap();
        let initially_closed = store
            .mutate(
                workspace,
                opened.intent.revision,
                AgentTabIntentMutation::Dismiss {
                    continuation: observed.continuation,
                },
            )
            .unwrap();
        let reopen_expected_revision = initially_closed.intent.revision;

        // Another TUI closes the same visible lineage after the Reopen client
        // loaded rev2. Its expected rev1 is stale and the key is already
        // dismissed, but this newer user action must still publish rev3.
        let newer_close = store
            .mutate(
                workspace,
                opened.intent.revision,
                AgentTabIntentMutation::DismissAndSelect {
                    continuation: observed.continuation,
                    session_id: None,
                    selected: None,
                },
            )
            .unwrap();
        assert!(newer_close.cas_conflict);
        assert!(newer_close.mutation_applied);
        assert_eq!(newer_close.intent.revision, reopen_expected_revision + 1);
        assert!(
            newer_close
                .intent
                .dismissed
                .contains(&observed.continuation)
        );
        let path = store.state_path(workspace);
        let bytes_after_close = fs::read(&path).unwrap();

        let stale_reopen = store
            .mutate(
                workspace,
                reopen_expected_revision,
                AgentTabIntentMutation::Reopen {
                    continuation: observed.continuation,
                },
            )
            .unwrap();
        assert!(stale_reopen.cas_conflict);
        assert!(!stale_reopen.mutation_applied);
        assert_eq!(stale_reopen.intent.revision, newer_close.intent.revision);
        assert!(
            stale_reopen
                .intent
                .dismissed
                .contains(&observed.continuation)
        );
        assert_eq!(fs::read(path).unwrap(), bytes_after_close);
    }

    #[test]
    fn stale_admission_cannot_change_identity_or_visibility_after_a_newer_close() {
        let (_root, mut store, workspace) = fixture();
        let observed = observation(workspace);
        let initial = store
            .mutate(
                workspace,
                0,
                AgentTabIntentMutation::Upsert {
                    session_id: None,
                    continuation: observed.continuation,
                    terminal: observed.terminal.clone(),
                    select: true,
                },
            )
            .unwrap();
        let closed = store
            .mutate(
                workspace,
                initial.intent.revision,
                AgentTabIntentMutation::Dismiss {
                    continuation: observed.continuation,
                },
            )
            .unwrap();
        let path = store.state_path(workspace);
        let bytes_before = fs::read(&path).unwrap();
        let replacement = observation(workspace).terminal;

        let stale = store
            .mutate(
                workspace,
                initial.intent.revision,
                AgentTabIntentMutation::Upsert {
                    session_id: None,
                    continuation: observed.continuation,
                    terminal: replacement.clone(),
                    select: true,
                },
            )
            .unwrap();

        assert!(stale.cas_conflict);
        assert!(!stale.mutation_applied);
        assert_eq!(stale.intent.revision, closed.intent.revision);
        assert!(stale.intent.dismissed.contains(&observed.continuation));
        assert!(
            stale.intent.targets[0].tabs[0]
                .terminal
                .fences(&observed.terminal)
        );
        assert!(
            !stale.intent.targets[0].tabs[0]
                .terminal
                .fences(&replacement)
        );
        assert_eq!(fs::read(path).unwrap(), bytes_before);
    }
}

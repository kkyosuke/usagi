//! Where one workspace's daemon state lives, and how a subtree is proven to
//! belong to that workspace.
//!
//! The daemon stays one process per machine while owning several workspaces, so
//! the state a *workspace* owns — the lifecycle document that binds a
//! `repository_root` and its root worktree identity — cannot stay in the single
//! `<data-dir>/daemon/` directory that holds one document per daemon. Each
//! workspace gets a subtree below [`paths::WORKSPACE_STATE_DIR`], named by a
//! shortened digest of its canonical root.
//!
//! A shortened digest can collide, so the name alone never decides ownership.
//! Every subtree carries a [`paths::WORKSPACE_STATE_ROOT_FILE`] naming the
//! canonical root it belongs to, and every reader compares it. A mismatch is a
//! miss — the caller probes the next candidate — so two workspaces can never
//! share one lifecycle document, whatever their digests do.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::infrastructure::paths;
use crate::infrastructure::persistence::json_file;

/// The legacy, data-directory-wide lifecycle document this module migrates.
const LEGACY_STATE_FILE: &str = "sessions.json";

/// The name a legacy document keeps after a subtree already holds the
/// authoritative one. The bytes stay for investigation; no build reads them.
const LEGACY_RETIRED_FILE: &str = "sessions.json.migrated";

/// The record left behind by [`migrate_legacy`].
const MIGRATION_RECORD_FILE: &str = "lifecycle-migration.json";

/// How many digest neighbours a resolution may try before giving up. Reaching
/// the end means the digest space around this workspace is genuinely occupied,
/// which is a fail-closed refusal rather than a reason to share a subtree.
const MAX_PROBE_ATTEMPTS: u32 = 16;

/// One workspace's state subtree, together with the root it is proven to hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceState {
    root: PathBuf,
    dir: PathBuf,
}

impl WorkspaceState {
    /// The canonical workspace root this subtree belongs to.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory holding this workspace's daemon-owned lifecycle state.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// The `root.json` payload. Only the root is recorded: everything else about a
/// workspace lives in the lifecycle document beside it.
#[derive(Debug, Serialize, Deserialize)]
struct RecordedRoot {
    root: PathBuf,
}

/// The `lifecycle-migration.json` payload, written once when a legacy document
/// moves into a subtree.
#[derive(Debug, Serialize, Deserialize)]
struct MigrationRecord {
    schema: String,
    root: PathBuf,
    moved_to: PathBuf,
    /// Whether the legacy document was retired instead of moved, because the
    /// subtree already held an authoritative one.
    retired: bool,
}

/// The field a migration keys on and rewrites. The rest of the document is
/// carried across as it stands.
const LEGACY_ROOT_FIELD: &str = "repository_root";

/// Resolve — creating it when absent — the state subtree of `workspace_root`.
///
/// `workspace_root` must already be canonical: the digest is taken over the
/// bytes of the path, so two spellings of one workspace would otherwise land on
/// two subtrees.
///
/// # Errors
///
/// Returns an error when a candidate subtree cannot be read or created, or when
/// [`MAX_PROBE_ATTEMPTS`] neighbours all belong to other workspaces.
pub fn resolve(daemon_dir: &Path, workspace_root: &Path) -> Result<WorkspaceState> {
    for attempt in 0..MAX_PROBE_ATTEMPTS {
        let dir = paths::workspace_state_dir(daemon_dir, workspace_root, attempt);
        match recorded_root(&dir)? {
            // A subtree that already names this workspace is the answer, and a
            // free candidate becomes it.
            Some(recorded) if recorded == workspace_root => {
                return Ok(WorkspaceState {
                    root: recorded,
                    dir,
                });
            }
            // A subtree that names another workspace is a digest collision:
            // the next candidate is tried rather than the state shared.
            Some(_) => {}
            None => {
                create_private_dir(&dir)?;
                let path = dir.join(paths::WORKSPACE_STATE_ROOT_FILE);
                let recorded = RecordedRoot {
                    root: workspace_root.to_path_buf(),
                };
                // The context is built eagerly, and the call that consumes it is
                // kept to one line. A lazy context would be a closure this crate
                // cannot make fail on demand, and a `?` on a line of its own is
                // only ever reached by the failure — both are invisible to a
                // suite that never sees this write fail.
                let failure = format!("could not record the root in {}", dir.display());
                json_file::write_atomic(&dir, &path, &recorded).context(failure)?;
                return Ok(WorkspaceState {
                    root: workspace_root.to_path_buf(),
                    dir,
                });
            }
        }
    }
    bail!(
        "no free daemon state subtree for the workspace {}",
        workspace_root.display()
    )
}

/// Every workspace that already has a state subtree below `daemon_dir`.
///
/// The listing is ordered by root so callers render a stable inventory. A
/// subtree that cannot be read is an error rather than a skipped entry: silently
/// ignoring it would let a second subtree be created for a workspace that
/// already owns one.
///
/// # Errors
///
/// Returns an error when the container or one of its subtrees cannot be read.
pub fn adopted(daemon_dir: &Path) -> Result<Vec<WorkspaceState>> {
    let container = daemon_dir.join(paths::WORKSPACE_STATE_DIR);
    let entries = match std::fs::read_dir(&container) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).context(format!("could not read {}", container.display()));
        }
    };
    let entries = entries
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()
        .context(format!("could not read {}", container.display()))?;
    let mut adopted = Vec::new();
    for dir in entries {
        // Only a real directory can be a subtree. A plain file or a symlink is
        // not something this build wrote, and reading through it would report a
        // filesystem error instead of the miss it actually is.
        if !dir.is_dir() {
            continue;
        }
        // A directory without a recorded root has no authority yet: a partially
        // created subtree must not be mistaken for an adopted workspace.
        if let Some(root) = recorded_root(&dir)? {
            adopted.push(WorkspaceState { root, dir });
        }
    }
    adopted.sort_by(|left, right| left.root.cmp(&right.root));
    Ok(adopted)
}

/// The adopted workspace that owns `candidate`: the one whose root is
/// `candidate` itself or its closest adopted ancestor.
///
/// Comparison is by path component, so `<root>-2` is never mistaken for a child
/// of `<root>`. The longest match wins, which is what lets a session worktree
/// (`<root>/.usagi/sessions/<name>`) resolve to its workspace even though the
/// worktree carries a `.usagi` directory of its own.
///
/// # Errors
///
/// Returns an error when the adopted subtrees cannot be read.
pub fn owner(daemon_dir: &Path, candidate: &Path) -> Result<Option<WorkspaceState>> {
    Ok(adopted(daemon_dir)?
        .into_iter()
        .filter(|state| candidate.starts_with(&state.root))
        .max_by_key(|state| state.root.components().count()))
}

/// Move a legacy `<daemon-dir>/sessions.json` into the subtree of the workspace
/// it names, once.
///
/// The document is carried across whole; the only field it changes is the
/// recorded `repository_root`, which is canonicalized (see below). The locator,
/// record, and locks it sat beside belong to the daemon rather than to the
/// workspace, so they stay where they are. Returns the subtree the document
/// landed in, or `None` when there is no legacy document left to move.
///
/// The document is written to its new home *before* the legacy copy is removed,
/// so an interrupted migration retries from the legacy position instead of
/// losing state. Two processes that reach an unmigrated data directory at the
/// same moment therefore converge: the second finds an authoritative document in
/// the subtree and retires the legacy bytes instead of overwriting them.
///
/// # Errors
///
/// Returns an error when the legacy document cannot be read, when its subtree
/// cannot be resolved, or when the move itself fails. A legacy document that
/// cannot be parsed fails closed here rather than being ignored: it names the
/// workspace this daemon would otherwise adopt.
pub fn migrate_legacy(daemon_dir: &Path) -> Result<Option<WorkspaceState>> {
    let legacy = daemon_dir.join(LEGACY_STATE_FILE);
    let Some(mut document) = json_file::read::<serde_json::Value>(&legacy)
        .with_context(|| format!("could not read {}", legacy.display()))?
    else {
        return Ok(None);
    };
    let recorded = document
        .get(LEGACY_ROOT_FIELD)
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .with_context(|| format!("{} records no workspace", legacy.display()))?;
    // The daemon keys everything — the subtree, the fence, the worktree paths —
    // on the *canonical* workspace root. A legacy document may record another
    // spelling of the same workspace: a symlinked ancestor, or the `/tmp` →
    // `/private/tmp` firmlink on macOS. Both the destination *and the recorded
    // field* are canonicalized, because either one left in the old spelling
    // splits the workspace in two: the daemon fences and opens one spelling while
    // the migrated document describes the other, and every session the document
    // held is orphaned beside it.
    //
    // A root that no longer resolves keeps its recorded spelling: the workspace
    // is gone, so there is no canonical form to prefer, and the bytes are still
    // worth moving out of the legacy position.
    let root = paths::canonical_workspace_root(&recorded).unwrap_or(recorded);
    if let Some(canonical) = root.to_str() {
        document[LEGACY_ROOT_FIELD] = serde_json::Value::String(canonical.to_owned());
    }
    let state = resolve(daemon_dir, &root)?;
    let target = state.dir.join(LEGACY_STATE_FILE);
    // A subtree that already holds a lifecycle document is authoritative: this
    // process must not overwrite it with an older whole-document snapshot. The
    // legacy bytes are retired beside the daemon instead, the way the runtime
    // migration retires the stores it replaced.
    let retired = target.exists();
    if retired {
        // The subtree already holds an authoritative document; the legacy bytes
        // are retired rather than written over it.
        let destination = daemon_dir.join(LEGACY_RETIRED_FILE);
        let failure = format!(
            "could not move {} to {}",
            legacy.display(),
            destination.display()
        );
        std::fs::rename(&legacy, &destination).context(failure)?;
    } else {
        // Written before the legacy copy is removed, so an interrupted migration
        // is retried from the legacy position rather than losing the document.
        let failure = format!("could not write {}", target.display());
        json_file::write_atomic(state.dir(), &target, &document).context(failure)?;
        let failure = format!("could not remove {}", legacy.display());
        std::fs::remove_file(&legacy).context(failure)?;
    }
    let record = MigrationRecord {
        schema: "usagi-lifecycle-migration-v1".into(),
        root: state.root.clone(),
        moved_to: state.dir.clone(),
        retired,
    };
    let path = daemon_dir.join(MIGRATION_RECORD_FILE);
    let failure = format!("could not record the migration in {}", daemon_dir.display());
    json_file::write_atomic(daemon_dir, &path, &record).context(failure)?;
    Ok(Some(state))
}

/// The canonical root a subtree records, or `None` when the subtree does not
/// exist or has no recorded root yet.
fn recorded_root(dir: &Path) -> Result<Option<PathBuf>> {
    let path = dir.join(paths::WORKSPACE_STATE_ROOT_FILE);
    Ok(json_file::read::<RecordedRoot>(&path)
        .with_context(|| format!("could not read {}", path.display()))?
        .map(|recorded| recorded.root))
}

/// Create a subtree with the daemon-private mode its parent already uses.
fn create_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).context(format!("could not create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .context(format!("could not restrict {}", dir.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn daemon_dir() -> tempfile::TempDir {
        tempfile::tempdir_in("/tmp").unwrap()
    }

    #[test]
    fn a_resolved_subtree_records_its_root_and_is_returned_again() {
        let daemon = daemon_dir();
        let root = PathBuf::from("/workspace/one");

        let first = resolve(daemon.path(), &root).unwrap();
        assert_eq!(first.root(), root);
        assert_eq!(
            first.dir(),
            paths::workspace_state_dir(daemon.path(), &root, 0)
        );

        // The recorded root makes the second resolution a lookup, not a second
        // subtree for the same workspace.
        assert_eq!(resolve(daemon.path(), &root).unwrap(), first);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(first.dir()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }

    #[test]
    fn a_taken_candidate_is_probed_past_rather_than_shared() {
        let daemon = daemon_dir();
        let root = PathBuf::from("/workspace/one");
        let intruder = PathBuf::from("/workspace/other");

        // Simulate a digest collision by recording another workspace under the
        // name this root would take.
        let taken = paths::workspace_state_dir(daemon.path(), &root, 0);
        create_private_dir(&taken).unwrap();
        json_file::write_atomic(
            &taken,
            &taken.join(paths::WORKSPACE_STATE_ROOT_FILE),
            &RecordedRoot {
                root: intruder.clone(),
            },
        )
        .unwrap();

        let state = resolve(daemon.path(), &root).unwrap();
        assert_eq!(state.root(), root);
        assert_eq!(
            state.dir(),
            paths::workspace_state_dir(daemon.path(), &root, 1)
        );
        // The intruder keeps its own subtree untouched.
        assert_eq!(recorded_root(&taken).unwrap(), Some(intruder));
    }

    #[test]
    fn an_exhausted_probe_refuses_instead_of_sharing() {
        let daemon = daemon_dir();
        let root = PathBuf::from("/workspace/one");
        for attempt in 0..MAX_PROBE_ATTEMPTS {
            let dir = paths::workspace_state_dir(daemon.path(), &root, attempt);
            create_private_dir(&dir).unwrap();
            json_file::write_atomic(
                &dir,
                &dir.join(paths::WORKSPACE_STATE_ROOT_FILE),
                &RecordedRoot {
                    root: PathBuf::from(format!("/workspace/other/{attempt}")),
                },
            )
            .unwrap();
        }

        let error = resolve(daemon.path(), &root).unwrap_err();
        assert!(
            format!("{error:#}").contains("no free daemon state subtree"),
            "{error:#}"
        );
    }

    #[test]
    fn a_corrupt_recorded_root_fails_closed() {
        let daemon = daemon_dir();
        let root = PathBuf::from("/workspace/one");
        let dir = paths::workspace_state_dir(daemon.path(), &root, 0);
        create_private_dir(&dir).unwrap();
        std::fs::write(dir.join(paths::WORKSPACE_STATE_ROOT_FILE), "not json").unwrap();

        let error = resolve(daemon.path(), &root).unwrap_err();
        assert!(format!("{error:#}").contains("root.json"), "{error:#}");
        let error = adopted(daemon.path()).unwrap_err();
        assert!(format!("{error:#}").contains("root.json"), "{error:#}");
    }

    #[test]
    fn adoption_lists_recorded_subtrees_and_ignores_the_rest() {
        let daemon = daemon_dir();
        assert!(adopted(daemon.path()).unwrap().is_empty());

        resolve(daemon.path(), Path::new("/workspace/two")).unwrap();
        resolve(daemon.path(), Path::new("/workspace/one")).unwrap();
        // A directory without a recorded root has no authority yet, and a plain
        // file in the container is not a subtree at all.
        create_private_dir(
            &daemon
                .path()
                .join(paths::WORKSPACE_STATE_DIR)
                .join("partial"),
        )
        .unwrap();
        std::fs::write(
            daemon.path().join(paths::WORKSPACE_STATE_DIR).join("stray"),
            "",
        )
        .unwrap();

        let roots: Vec<_> = adopted(daemon.path())
            .unwrap()
            .into_iter()
            .map(|state| state.root)
            .collect();
        assert_eq!(
            roots,
            vec![
                PathBuf::from("/workspace/one"),
                PathBuf::from("/workspace/two")
            ]
        );
    }

    #[test]
    fn an_unreadable_container_is_reported() {
        let daemon = daemon_dir();
        // A file where the container belongs cannot be enumerated, and pretending
        // it is empty would let a second subtree be created for every workspace.
        std::fs::write(daemon.path().join(paths::WORKSPACE_STATE_DIR), "").unwrap();
        let error = adopted(daemon.path()).unwrap_err();
        assert!(format!("{error:#}").contains("could not read"), "{error:#}");
    }

    #[test]
    fn the_owner_of_a_path_is_its_closest_adopted_ancestor() {
        let daemon = daemon_dir();
        resolve(daemon.path(), Path::new("/workspace/one")).unwrap();
        resolve(daemon.path(), Path::new("/workspace/one/nested")).unwrap();

        // Exact match, ancestor match, and longest-match-wins.
        for (candidate, expected) in [
            ("/workspace/one", "/workspace/one"),
            ("/workspace/one/.usagi/sessions/work", "/workspace/one"),
            ("/workspace/one/nested/deep", "/workspace/one/nested"),
        ] {
            let owner = owner(daemon.path(), Path::new(candidate)).unwrap().unwrap();
            assert_eq!(owner.root(), Path::new(expected), "{candidate}");
        }

        // A sibling that merely shares a prefix in spelling is not a child, and
        // an unadopted workspace has no owner at all.
        assert_eq!(
            owner(daemon.path(), Path::new("/workspace/one-2")).unwrap(),
            None
        );
        assert_eq!(owner(daemon.path(), Path::new("/elsewhere")).unwrap(), None);
    }

    #[test]
    fn a_legacy_document_moves_into_its_workspace_subtree_once() {
        let daemon = daemon_dir();
        let root = PathBuf::from("/workspace/one");
        let legacy = daemon.path().join(LEGACY_STATE_FILE);
        std::fs::write(
            &legacy,
            r#"{"repository_root":"/workspace/one","state":{}}"#,
        )
        .unwrap();

        let state = migrate_legacy(daemon.path()).unwrap().unwrap();
        assert_eq!(state.root(), root);
        assert!(!legacy.exists());
        assert!(state.dir().join(LEGACY_STATE_FILE).exists());
        let record: MigrationRecord = json_file::read(&daemon.path().join(MIGRATION_RECORD_FILE))
            .unwrap()
            .unwrap();
        assert_eq!(record.root, root);
        assert!(!record.retired);

        // The migration is one-way: with nothing left to move it is a no-op.
        assert_eq!(migrate_legacy(daemon.path()).unwrap(), None);
    }

    /// A legacy document may record a different spelling of the same workspace:
    /// a symlinked ancestor, or the `/tmp` → `/private/tmp` firmlink on macOS.
    /// The daemon keys the subtree by the canonical root, so the migration must
    /// too — otherwise the daemon opens an empty subtree beside the migrated one
    /// and every session the document held is silently orphaned.
    #[test]
    fn a_legacy_document_moves_to_the_canonical_spelling_of_its_workspace() {
        let daemon = daemon_dir();
        let workspace = tempfile::tempdir_in("/tmp").unwrap();
        let real = workspace.path().join("workspace");
        std::fs::create_dir(&real).unwrap();
        let link = workspace.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let canonical = paths::canonical_workspace_root(&real).unwrap();

        // The document records the workspace through the symlink.
        let legacy = daemon.path().join(LEGACY_STATE_FILE);
        std::fs::write(
            &legacy,
            format!(
                r#"{{"repository_root":{:?},"root_worktree_id":"8adf6382-cb49-4446-9323-560cad878712","state":{{"kept":true}}}}"#,
                link.to_str().unwrap()
            ),
        )
        .unwrap();

        let state = migrate_legacy(daemon.path()).unwrap().unwrap();
        assert_eq!(state.root(), canonical);
        assert_eq!(
            state.dir(),
            paths::workspace_state_dir(daemon.path(), &canonical, 0)
        );
        // The recorded root is canonical too. Leaving the old spelling in the
        // document would split the workspace: the daemon keys its fence, subtree,
        // and worktree paths on what the document says, so it would open an empty
        // subtree beside this one and orphan everything the document holds.
        let migrated: serde_json::Value = json_file::read(&state.dir().join(LEGACY_STATE_FILE))
            .unwrap()
            .unwrap();
        assert_eq!(
            migrated["repository_root"].as_str(),
            canonical.to_str(),
            "the migrated document must describe the canonical workspace"
        );
        // And it is the subtree a later resolution of the same workspace finds,
        // which is the whole point: the daemon must open the migrated document.
        assert_eq!(resolve(daemon.path(), &canonical).unwrap(), state);
        // Everything else in the document survives the rewrite.
        assert_eq!(migrated["state"]["kept"], serde_json::json!(true));
        assert_eq!(
            migrated["root_worktree_id"].as_str(),
            Some("8adf6382-cb49-4446-9323-560cad878712")
        );
    }

    #[test]
    fn a_legacy_document_is_retired_when_the_subtree_already_holds_one() {
        let daemon = daemon_dir();
        let root = PathBuf::from("/workspace/one");
        let state = resolve(daemon.path(), &root).unwrap();
        std::fs::write(
            state.dir().join(LEGACY_STATE_FILE),
            r#"{"authoritative":true}"#,
        )
        .unwrap();
        let legacy = daemon.path().join(LEGACY_STATE_FILE);
        std::fs::write(&legacy, r#"{"repository_root":"/workspace/one"}"#).unwrap();

        let migrated = migrate_legacy(daemon.path()).unwrap().unwrap();
        assert_eq!(migrated, state);
        assert!(!legacy.exists());
        assert_eq!(
            std::fs::read_to_string(state.dir().join(LEGACY_STATE_FILE)).unwrap(),
            r#"{"authoritative":true}"#
        );
        assert!(daemon.path().join(LEGACY_RETIRED_FILE).exists());
        let record: MigrationRecord = json_file::read(&daemon.path().join(MIGRATION_RECORD_FILE))
            .unwrap()
            .unwrap();
        assert!(record.retired);
    }

    #[test]
    fn a_corrupt_legacy_document_fails_closed() {
        let daemon = daemon_dir();
        std::fs::write(daemon.path().join(LEGACY_STATE_FILE), "not json").unwrap();
        let error = migrate_legacy(daemon.path()).unwrap_err();
        assert!(format!("{error:#}").contains("sessions.json"), "{error:#}");

        // A document that parses but names no workspace is the same kind of
        // failure: it is the only thing that says which workspace this state
        // belongs to, so guessing would attach it to the wrong one.
        std::fs::write(
            daemon.path().join(LEGACY_STATE_FILE),
            r#"{"state":{"sessions":[]}}"#,
        )
        .unwrap();
        let error = migrate_legacy(daemon.path()).unwrap_err();
        assert!(
            format!("{error:#}").contains("records no workspace"),
            "{error:#}"
        );
    }

    #[test]
    fn a_subtree_that_cannot_be_resolved_stops_the_migration() {
        let daemon = daemon_dir();
        let root = daemon.path().join("workspace");
        std::fs::write(
            daemon.path().join(LEGACY_STATE_FILE),
            format!(r#"{{"repository_root":{:?}}}"#, root.to_str().unwrap()),
        )
        .unwrap();
        // Replace the resolved subtree with a file so the rename into it fails.
        let subtree = paths::workspace_state_dir(daemon.path(), &root, 0);
        resolve(daemon.path(), &root).unwrap();
        std::fs::remove_dir_all(&subtree).unwrap();
        std::fs::write(&subtree, "").unwrap();

        let error = migrate_legacy(daemon.path()).unwrap_err();
        assert!(format!("{error:#}").contains("could not read"), "{error:#}");
    }
}

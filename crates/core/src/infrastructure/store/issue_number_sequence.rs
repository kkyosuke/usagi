//! Workspace-global, crash-durable issue number reservations.
//!
//! Issue Markdown lives in each Git worktree, so a per-store lock cannot
//! serialize allocation across sibling worktrees. The one authority shared
//! with the production v1 allocator lives below Git's common directory and
//! combines a high-water sequence with durable per-number reservation markers.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::infrastructure::paths::{SESSIONS_DIR, STATE_DIR};
use crate::infrastructure::persistence::json_file::{self, write_text_atomic};
use crate::infrastructure::persistence::store_lock::StoreLock;

const AUTHORITY_PARENT_DIR: &str = "usagi";
const AUTHORITY_DIR: &str = "issue-numbers";
const SEQUENCE_FILE: &str = "sequence.json";
const RESERVATIONS_DIR: &str = "reservations";
const RESERVATION_SUFFIX: &str = ".reserved";
const SEQUENCE_VERSION: u32 = 1;

const LEGACY_V2_DIR: &str = "usagi-issue-sequence";
const LEGACY_V2_FILE: &str = "next";
const LEGACY_V2_MIGRATION_FILE: &str = "legacy-v2-migrated";
const LEGACY_V2_SENTINEL_PREFIX: &str = "migrated-to-usagi-issue-numbers:";
const REPO_SCOPING_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_COMMON_DIR",
    "GIT_PREFIX",
    "GIT_NAMESPACE",
];

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SequenceFile {
    version: u32,
    last_reserved: u32,
    /// Present only while `last_reserved == u32::MAX`. Old v1 ignores this
    /// additional field and fails its checked increment, while fixed v2 uses
    /// it to recover the real high-water after a crash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    migration_floor: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SequenceState {
    Normal(u32),
    MigrationBlocked(u32),
}

impl SequenceState {
    fn floor(self) -> u32 {
        match self {
            Self::Normal(floor) | Self::MigrationBlocked(floor) => floor,
        }
    }

    fn is_blocked(self) -> bool {
        matches!(self, Self::MigrationBlocked(_))
    }

    fn stops_old_v1(self) -> bool {
        self.is_blocked() || self == Self::Normal(u32::MAX)
    }
}

struct GitRepository {
    worktree_root: PathBuf,
    common_dir: PathBuf,
}

enum LegacyScope {
    Git {
        shared_sequence: PathBuf,
        workspace_root: PathBuf,
        worktree_root: PathBuf,
        current_is_nested: bool,
        current_issue_store: PathBuf,
    },
    NonGit {
        workspace_root: PathBuf,
        current_issue_store: PathBuf,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LegacyState {
    /// No old authority has written this path yet.
    Missing,
    /// Old-v2-compatible plain `u32` state.
    Active(u32),
    /// New-v2-readable state deliberately rejected by old v2's `u32` parser.
    Fenced(u32),
}

impl LegacyState {
    fn floor(self) -> u32 {
        match self {
            Self::Missing => 0,
            Self::Active(floor) | Self::Fenced(floor) => floor,
        }
    }
}

/// The process/worktree-shared authority that reserves issue numbers.
pub(crate) struct IssueNumberSequence {
    dir: PathBuf,
    worktree_root: PathBuf,
    legacy_scope: LegacyScope,
}

/// Source maxima observed by fixed v2 and, only when proven by a controlled
/// caller, by every compatible old-v1 allocator. Production Git callers set
/// `v1_visible` to zero because no source tree is visible from every possible
/// linked-worktree cwd sharing the common authority.
#[derive(Debug)]
pub(crate) struct ExistingIssueFloors {
    pub(crate) all: u32,
    pub(crate) v1_visible: u32,
}

impl ExistingIssueFloors {
    #[cfg(test)]
    fn shared(floor: u32) -> Self {
        Self {
            all: floor,
            v1_visible: floor,
        }
    }
}

impl IssueNumberSequence {
    /// Resolve the v1-compatible authority and every old-v2 authority that must
    /// be folded and fenced.
    ///
    /// Git repositories use the nearest ancestor worktree boundary and Git's
    /// validated common directory. Non-Git workspaces use `.usagi` and retain
    /// the current store-local legacy location for compatibility.
    pub(crate) fn new(
        repo_root: &Path,
        workspace_root: &Path,
        issue_store_dir: &Path,
    ) -> Result<Self> {
        if let Some(repository) = git_repository(repo_root)? {
            validate_conventional_workspace_repository(&repository, workspace_root)?;
            let normalized_repo_root = normalize_path_identity(repo_root)?;
            let mut fallback_roots = vec![
                workspace_root.to_path_buf(),
                repository.worktree_root.clone(),
                normalized_repo_root.clone(),
            ];
            fallback_roots.extend(git_worktree_roots(&repository.worktree_root)?);
            fallback_roots.sort();
            fallback_roots.dedup();
            for fallback_root in fallback_roots {
                let fallback_authority = fallback_root.join(STATE_DIR).join(AUTHORITY_DIR);
                ensure_authority_absent(&fallback_authority).context(format!(
                    "a pre-Git issue-number authority must be reconciled before using Git authority {}",
                    fallback_authority.display()
                ))?;
            }
            let common = repository.common_dir;
            let worktree_root = repository.worktree_root;
            let current_is_nested = normalized_repo_root != worktree_root;
            return Ok(Self {
                dir: common.join(AUTHORITY_PARENT_DIR).join(AUTHORITY_DIR),
                worktree_root: worktree_root.clone(),
                legacy_scope: LegacyScope::Git {
                    shared_sequence: common.join(LEGACY_V2_DIR).join(LEGACY_V2_FILE),
                    workspace_root: workspace_root.to_path_buf(),
                    worktree_root,
                    current_is_nested,
                    current_issue_store: issue_store_dir.to_path_buf(),
                },
            });
        }

        Ok(Self {
            dir: workspace_root.join(STATE_DIR).join(AUTHORITY_DIR),
            worktree_root: repo_root.to_path_buf(),
            legacy_scope: LegacyScope::NonGit {
                workspace_root: workspace_root.to_path_buf(),
                current_issue_store: issue_store_dir.to_path_buf(),
            },
        })
    }

    /// Directory containing the authority lock, sequence, and journal.
    #[cfg(test)]
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Repository worktree root normalized from an arbitrary nested cwd.
    pub(crate) fn worktree_root(&self) -> &Path {
        &self.worktree_root
    }

    pub(crate) fn registered_worktrees(&self) -> Result<Vec<PathBuf>> {
        if self.is_git_shared() {
            git_worktree_roots(&self.worktree_root)
        } else {
            Ok(Vec::new())
        }
    }

    pub(crate) fn materialized_git_issue_roots(&self) -> Result<Vec<PathBuf>> {
        if !self.is_git_shared() {
            return Ok(Vec::new());
        }
        let mut roots = Vec::new();
        for worktree in git_worktree_roots(&self.worktree_root)? {
            push_materialized_git_issue_roots(&mut roots, &worktree)?;
        }
        roots.sort();
        roots.dedup();
        Ok(roots)
    }

    fn sequence_path(&self) -> PathBuf {
        self.dir.join(SEQUENCE_FILE)
    }

    fn reservations_dir(&self) -> PathBuf {
        self.dir.join(RESERVATIONS_DIR)
    }

    fn legacy_v2_migration_path(&self) -> PathBuf {
        self.dir.join(LEGACY_V2_MIGRATION_FILE)
    }

    fn is_git_shared(&self) -> bool {
        matches!(self.legacy_scope, LegacyScope::Git { .. })
    }

    /// Reserve one number while holding both the new authority lock and every
    /// relevant old-v2 lock in that fixed order.
    ///
    /// For a fresh migration, the first write fences old v2 when old v1 can see
    /// every durable floor, or blocks old v1 when the sole live legacy path can
    /// see every durable floor. If neither side can, migration fails before an
    /// authoritative write. Every legacy path is then fenced and the
    /// reservation is committed before the normal v1 sequence is restored.
    #[cfg(test)]
    pub(crate) fn reserve<F>(&self, max_existing: F) -> Result<u32>
    where
        F: FnMut() -> Result<u32>,
    {
        self.reserve_observing(max_existing, || {})
    }

    pub(crate) fn reserve_with_floors<F>(&self, existing_floors: F) -> Result<u32>
    where
        F: FnMut() -> Result<ExistingIssueFloors>,
    {
        self.reserve_observing_floors(existing_floors, || {})
    }

    #[cfg(test)]
    fn reserve_observing<F, O>(&self, mut max_existing: F, migration_blocked: O) -> Result<u32>
    where
        F: FnMut() -> Result<u32>,
        O: FnMut(),
    {
        self.reserve_observing_floors(
            || max_existing().map(ExistingIssueFloors::shared),
            migration_blocked,
        )
    }

    fn reserve_observing_floors<F, O>(
        &self,
        mut existing_floors: F,
        mut migration_blocked: O,
    ) -> Result<u32>
    where
        F: FnMut() -> Result<ExistingIssueFloors>,
        O: FnMut(),
    {
        self.reserve_observing_floors_dyn(&mut existing_floors, &mut migration_blocked)
    }

    fn reserve_observing_floors_dyn(
        &self,
        existing_floors: &mut dyn FnMut() -> Result<ExistingIssueFloors>,
        migration_blocked: &mut dyn FnMut(),
    ) -> Result<u32> {
        let _authority_lock = StoreLock::acquire(&self.dir)?;
        let legacy_paths = self.legacy_v2_sequences()?;
        let _legacy_locks = acquire_legacy_locks(&legacy_paths)?;

        // Validate every authority input before the first authoritative write.
        let existing = existing_floors()?;
        ensure!(
            existing.v1_visible <= existing.all,
            "v1-visible issue floor exceeds the complete source floor"
        );
        let sequence = self.read_sequence()?;
        let migration = self.read_legacy_v2_migration()?;
        let journal = self.max_reservation()?;
        let legacy_states = legacy_paths
            .iter()
            .map(|path| Self::read_legacy_v2_sequence(path).map(|state| (path, state)))
            .collect::<Result<Vec<_>>>()?;

        if self.is_git_shared()
            && let Some(migration_floor) = migration
        {
            let shared = self
                .shared_legacy_sequence()
                .context("Git issue allocation is missing its shared legacy authority")?;
            let shared_state = legacy_states
                .iter()
                .find_map(|(path, state)| (*path == shared).then_some(*state))
                .context("Git issue allocation did not inspect its shared legacy authority")?;
            ensure!(
                shared_state == LegacyState::Fenced(migration_floor) || sequence.stops_old_v1(),
                "shared legacy v2 issue sequence fence disagrees with migration marker: {}",
                shared.display()
            );
        }

        let legacy_floor = legacy_states
            .iter()
            .map(|(_, state)| state.floor())
            .max()
            .unwrap_or(0);
        let v1_visible_floor = existing
            .v1_visible
            .max(match sequence {
                SequenceState::Normal(floor) => floor,
                SequenceState::MigrationBlocked(_) => 0,
            })
            .max(journal);
        let floor = existing
            .all
            .max(v1_visible_floor)
            .max(sequence.floor())
            .max(migration.unwrap_or(0))
            .max(legacy_floor);

        let git_marker_incomplete = self.is_git_shared() && migration.is_none();
        let fences_match_git_marker = migration.is_some_and(|migration_floor| {
            legacy_states
                .iter()
                .all(|(_, state)| *state == LegacyState::Fenced(migration_floor))
        });
        let needs_migration = sequence.is_blocked()
            || git_marker_incomplete
            || if self.is_git_shared() {
                !fences_match_git_marker
            } else {
                legacy_states
                    .iter()
                    .any(|(_, state)| !matches!(state, LegacyState::Fenced(_)))
            };

        let terminal_exhaustion = floor == u32::MAX
            && sequence == SequenceState::Normal(u32::MAX)
            && legacy_states
                .iter()
                .all(|(_, state)| *state == LegacyState::Fenced(u32::MAX))
            && (!self.is_git_shared() || migration == Some(u32::MAX));
        if terminal_exhaustion {
            bail!("cannot allocate another issue number because the u32 range is exhausted");
        }

        if floor == u32::MAX {
            self.finish_exhausted_migration(sequence, v1_visible_floor, &legacy_states)?;
            bail!("cannot allocate another issue number because the u32 range is exhausted");
        }

        if !needs_migration {
            let number = floor
                .checked_add(1)
                .context("a non-exhausted issue floor must have a following number")?;
            self.write_reservation(number)?;
            return Ok(number);
        }

        self.migrate_and_reserve(
            floor,
            sequence,
            v1_visible_floor,
            &legacy_states,
            migration_blocked,
        )
    }

    fn finish_exhausted_migration(
        &self,
        sequence: SequenceState,
        v1_visible_floor: u32,
        legacy_states: &[(&PathBuf, LegacyState)],
    ) -> Result<()> {
        let unfenced = single_unfenced_legacy(legacy_states)?;
        if let Some((path, state)) = unfenced {
            if sequence.is_blocked() || v1_visible_floor == u32::MAX {
                write_legacy_sentinel(path, u32::MAX)?;
            } else {
                ensure!(
                    state.floor() == u32::MAX,
                    "neither live legacy v2 nor v1 can see the durable issue floor; stop old writers and reconcile every authority before retrying"
                );
                self.write_sequence(u32::MAX)?;
            }
        } else if !sequence.is_blocked() && v1_visible_floor != u32::MAX {
            self.write_sequence(u32::MAX)?;
        }

        // This terminal sequence is the recovery tag for any following partial
        // sentinel/marker write as well as an old-v1 stop.
        self.write_sequence(u32::MAX)?;
        for (path, _) in legacy_states {
            write_legacy_sentinel(path, u32::MAX)?;
        }
        if self.is_git_shared() {
            write_text_atomic(&self.legacy_v2_migration_path(), &format!("{}\n", u32::MAX))?;
        }
        self.write_sequence(u32::MAX)
    }

    fn migrate_and_reserve(
        &self,
        floor: u32,
        sequence: SequenceState,
        v1_visible_floor: u32,
        legacy_states: &[(&PathBuf, LegacyState)],
        migration_blocked: &mut dyn FnMut(),
    ) -> Result<u32> {
        let number = floor
            .checked_add(1)
            .context("a non-exhausted issue floor must have a following number")?;
        let unfenced = single_unfenced_legacy(legacy_states)?;

        if sequence.is_blocked() {
            if let Some((path, _)) = unfenced {
                write_legacy_sentinel(path, floor)?;
            }
        } else if let Some((path, state)) = unfenced {
            if v1_visible_floor == floor {
                write_legacy_sentinel(path, floor)?;
            } else {
                ensure!(
                    state.floor() == floor,
                    "neither live legacy v2 nor v1 can see the durable issue floor; stop old writers and reconcile every authority before retrying"
                );
            }
        }

        self.write_migration_blocker(floor)?;
        migration_blocked();
        let mut fence_paths: Vec<_> = legacy_states.iter().collect();
        fence_paths.sort_by_key(|(path, state)| {
            let priority = match state {
                LegacyState::Active(_) => 0,
                LegacyState::Missing => 1,
                LegacyState::Fenced(_) => 2,
            };
            (priority, *path)
        });
        for (path, _) in fence_paths {
            write_legacy_sentinel(path, number)?;
        }
        self.write_reservation_marker(number)?;
        if self.is_git_shared() {
            write_text_atomic(&self.legacy_v2_migration_path(), &format!("{number}\n"))?;
        }
        self.write_sequence(number)?;
        Ok(number)
    }

    fn write_reservation(&self, number: u32) -> Result<()> {
        self.write_reservation_marker(number)?;
        self.write_sequence(number)
    }

    fn write_reservation_marker(&self, number: u32) -> Result<()> {
        let reservations = self.reservations_dir();
        fs::create_dir_all(&reservations)
            .context(format!("failed to create {}", reservations.display()))?;
        let marker = reservations.join(reservation_name(number));
        write_text_atomic(&marker, &format!("{number}\n"))
    }

    fn write_sequence(&self, number: u32) -> Result<()> {
        json_file::write_atomic(
            &self.dir,
            &self.sequence_path(),
            &SequenceFile {
                version: SEQUENCE_VERSION,
                last_reserved: number,
                migration_floor: None,
            },
        )
    }

    fn write_migration_blocker(&self, floor: u32) -> Result<()> {
        ensure!(
            floor < u32::MAX,
            "cannot block issue sequence migration after the u32 range is exhausted"
        );
        json_file::write_atomic(
            &self.dir,
            &self.sequence_path(),
            &SequenceFile {
                version: SEQUENCE_VERSION,
                last_reserved: u32::MAX,
                migration_floor: Some(floor),
            },
        )
    }

    /// Missing means an uninitialized/migration state. Existing malformed or
    /// unreadable data is never guessed around because it may be the high-water
    /// mark that prevents a duplicate allocation.
    fn read_sequence(&self) -> Result<SequenceState> {
        let path = self.sequence_path();
        let Some(text) = read_optional_text(&path, "issue sequence")? else {
            return Ok(SequenceState::Normal(0));
        };
        let sequence: SequenceFile =
            serde_json::from_str(&text).context(format!("failed to parse {}", path.display()))?;
        ensure!(
            sequence.version == SEQUENCE_VERSION,
            "unsupported issue sequence version {} in {}",
            sequence.version,
            path.display()
        );
        match (sequence.last_reserved, sequence.migration_floor) {
            (u32::MAX, Some(floor)) if floor < u32::MAX => {
                Ok(SequenceState::MigrationBlocked(floor))
            }
            (u32::MAX, Some(_)) => bail!(
                "issue sequence has an invalid migration floor: {}",
                path.display()
            ),
            (last_reserved, None) => Ok(SequenceState::Normal(last_reserved)),
            (_, Some(_)) => bail!(
                "issue sequence has a migration floor without the u32::MAX blocker: {}",
                path.display()
            ),
        }
    }

    fn read_legacy_v2_sequence(path: &Path) -> Result<LegacyState> {
        let Some(text) = read_optional_text(path, "legacy v2 issue sequence")? else {
            return Ok(LegacyState::Missing);
        };
        if let Some(number) = parse_legacy_sentinel(&text) {
            return Ok(LegacyState::Fenced(number?));
        }
        text.trim()
            .parse::<u32>()
            .map(LegacyState::Active)
            .context(format!(
                "invalid legacy v2 issue sequence in {}",
                path.display()
            ))
    }

    /// The Git marker is written only after the legacy sentinel. Non-Git code
    /// does not publish it, but still folds a pre-existing marker produced by an
    /// interrupted development build rather than silently moving backwards.
    fn read_legacy_v2_migration(&self) -> Result<Option<u32>> {
        let path = self.legacy_v2_migration_path();
        let Some(text) = read_optional_text(&path, "legacy v2 issue migration")? else {
            return Ok(None);
        };
        let number = text.trim().parse::<u32>().context(format!(
            "invalid legacy v2 issue migration in {}",
            path.display()
        ))?;
        ensure!(
            text == format!("{number}\n"),
            "invalid legacy v2 issue migration format in {}",
            path.display()
        );
        Ok(Some(number))
    }

    fn max_reservation(&self) -> Result<u32> {
        let reservations = self.reservations_dir();
        let entries = match fs::read_dir(&reservations) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if path_is_missing(&reservations)? {
                    return Ok(0);
                }
                return Err(error).context(format!(
                    "failed to read issue reservations {}",
                    reservations.display()
                ));
            }
            Err(error) => {
                return Err(error).context(format!(
                    "failed to read issue reservations {}",
                    reservations.display()
                ));
            }
        };
        let mut max = 0;
        for entry in entries {
            let entry = entry.context("failed to read an issue reservation entry")?;
            let path = entry.path();
            let name = entry.file_name();
            let invalid_name = format!(
                "issue reservation filename is not UTF-8: {}",
                path.display()
            );
            let name = name.to_str().context(invalid_name)?;
            let Some(stem) = name.strip_suffix(RESERVATION_SUFFIX) else {
                continue;
            };
            let number = stem.parse::<u32>().context(format!(
                "invalid issue reservation marker name {}",
                path.display()
            ))?;
            ensure!(
                name == reservation_name(number),
                "non-canonical issue reservation marker name {}",
                path.display()
            );
            let text = fs::read_to_string(&path).context(format!(
                "failed to read issue reservation {}",
                path.display()
            ))?;
            ensure!(
                text == format!("{number}\n"),
                "invalid issue reservation marker {}: filename and contents disagree",
                path.display()
            );
            max = max.max(number);
        }
        Ok(max)
    }

    fn legacy_v2_sequences(&self) -> Result<Vec<PathBuf>> {
        let paths = match &self.legacy_scope {
            LegacyScope::Git {
                shared_sequence,
                workspace_root,
                worktree_root,
                current_is_nested,
                current_issue_store,
            } => {
                let mut paths = vec![shared_sequence.clone()];
                if *current_is_nested {
                    push_store_legacy(&mut paths, current_issue_store);
                } else {
                    push_existing_store_legacy(&mut paths, current_issue_store)?;
                }
                push_existing_store_legacy(
                    &mut paths,
                    &worktree_root.join(STATE_DIR).join("issues"),
                )?;
                push_existing_store_legacy(
                    &mut paths,
                    &workspace_root.join(STATE_DIR).join("issues"),
                )?;
                push_git_session_legacies(&mut paths, worktree_root, workspace_root)?;
                push_materialized_git_legacies_in_all_worktrees(&mut paths, worktree_root)?;
                push_source_derived_git_legacies(&mut paths, worktree_root)?;
                paths
            }
            LegacyScope::NonGit {
                workspace_root,
                current_issue_store,
            } => {
                let mut paths = Vec::new();
                push_store_legacy(&mut paths, &workspace_root.join(STATE_DIR).join("issues"));
                push_store_legacy(&mut paths, current_issue_store);
                push_all_session_legacies(&mut paths, workspace_root)?;
                paths
            }
        };
        deduplicate_legacy_paths(paths)
    }

    fn shared_legacy_sequence(&self) -> Option<&Path> {
        match &self.legacy_scope {
            LegacyScope::Git {
                shared_sequence, ..
            } => Some(shared_sequence),
            LegacyScope::NonGit { .. } => None,
        }
    }
}

fn single_unfenced_legacy<'a>(
    legacy_states: &[(&'a PathBuf, LegacyState)],
) -> Result<Option<(&'a PathBuf, LegacyState)>> {
    let mut unfenced = legacy_states
        .iter()
        .copied()
        .filter(|(_, state)| !matches!(state, LegacyState::Fenced(_)));
    let first = unfenced.next();
    ensure!(
        unfenced.next().is_none(),
        "multiple independent legacy v2 issue authorities are not fenced; stop every pre-fix writer and reconcile all durable floors into one authority before retrying"
    );
    Ok(first)
}

fn reservation_name(number: u32) -> String {
    format!("{number:010}{RESERVATION_SUFFIX}")
}

fn ensure_authority_absent(path: &Path) -> Result<()> {
    let path_display = path.display().to_string();
    match fs::symlink_metadata(path) {
        Ok(_) => bail!("alternate non-Git issue-number authority still exists at {path_display}"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure!(
                path_is_missing(path)?,
                "alternate non-Git issue-number authority is dangling or unreadable at {path_display}"
            );
            Ok(())
        }
        Err(error) => Err(error).context(format!(
            "failed to inspect alternate issue-number authority {path_display}"
        )),
    }
}

fn legacy_sequence_for_store(store: &Path) -> PathBuf {
    store.join(LEGACY_V2_DIR).join(LEGACY_V2_FILE)
}

fn push_store_legacy(paths: &mut Vec<PathBuf>, store: &Path) {
    paths.push(legacy_sequence_for_store(store));
}

fn push_existing_store_legacy(paths: &mut Vec<PathBuf>, store: &Path) -> Result<()> {
    let sequence = legacy_sequence_for_store(store);
    let dir = sequence
        .parent()
        .context("a legacy store sequence must have a parent")?;
    let dir_display = dir.display().to_string();
    match fs::metadata(dir) {
        Ok(metadata) => {
            ensure!(
                metadata.is_dir(),
                "legacy issue authority is not a directory: {dir_display}"
            );
            paths.push(sequence);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure!(
                path_is_missing(dir)?,
                "legacy issue authority is an unreadable or dangling path: {dir_display}"
            );
        }
        Err(error) => {
            return Err(error).context(format!(
                "failed to inspect legacy issue authority {dir_display}"
            ));
        }
    }
    Ok(())
}

fn read_optional_text(path: &Path, label: &str) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if path_is_missing(path)? {
                Ok(None)
            } else {
                Err(error).context(format!("failed to read {label} {}", path.display()))
            }
        }
        Err(error) => Err(error).context(format!("failed to read {label} {}", path.display())),
    }
}

fn path_is_missing(path: &Path) -> Result<bool> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                return Ok(metadata.is_dir() && !metadata.file_type().is_symlink());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).context(format!(
                    "failed to inspect authoritative path {}",
                    ancestor.display()
                ));
            }
        }
    }
    Ok(false)
}

fn deduplicate_legacy_paths(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    let mut identities = std::collections::BTreeSet::new();
    for path in paths {
        let parent = path.parent().context(format!(
            "legacy issue sequence has no parent: {}",
            path.display()
        ))?;
        let name = path.file_name().context(format!(
            "legacy issue sequence has no filename: {}",
            path.display()
        ))?;
        // Canonicalize only the parent identity. Following the `next` leaf
        // itself could hide a dangling or redirected authoritative symlink.
        let identity = normalize_path_identity(parent)?.join(name);
        identities.insert(identity);
    }
    // Return the normalized identities themselves. Sorting raw aliases before
    // deduplication lets two callers acquire the same lock set in a different
    // order; the BTreeSet fixes one cross-process order for every alias.
    Ok(identities.into_iter().collect())
}

fn normalize_path_identity(path: &Path) -> Result<PathBuf> {
    let mut missing = Vec::new();
    let mut cursor = path;
    loop {
        match fs::canonicalize(cursor) {
            Ok(mut canonical) => {
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let missing_name = format!("cannot normalize missing path {}", path.display());
                let name = cursor.file_name().context(missing_name)?;
                missing.push(name.to_os_string());
                let missing_ancestor = format!(
                    "cannot normalize path without an existing ancestor: {}",
                    path.display()
                );
                cursor = cursor.parent().context(missing_ancestor)?;
            }
            Err(error) => {
                return Err(error).context(format!("failed to normalize path {}", path.display()));
            }
        }
    }
}

fn push_git_session_legacies(
    paths: &mut Vec<PathBuf>,
    repository: &Path,
    workspace_root: &Path,
) -> Result<()> {
    let registered = git_worktree_roots(repository)?;
    let registered_identities = registered
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let mut parents = registered.clone();
    parents.push(normalize_path_identity(workspace_root)?);
    parents.sort();
    parents.dedup();
    for parent in parents {
        for session in direct_session_roots(&parent)? {
            let store = session.join(STATE_DIR).join("issues");
            if registered_identities.contains(&normalize_path_identity(&session)?) {
                // A registered worktree root made old v2 follow its `.git`
                // file to the shared legacy authority. Still fold an explicitly
                // materialized local authority, but do not invent a missing one.
                push_existing_store_legacy(paths, &store)?;
            } else {
                ensure_no_independent_git_authority(&session)?;
                // An ordinary direct session has no `.git`, so pre-fix v2 used
                // this store-local authority even before its first `next` write.
                push_store_legacy(paths, &store);
            }
        }
    }
    Ok(())
}

fn ensure_no_independent_git_authority(session: &Path) -> Result<()> {
    let dot_git = session.join(".git");
    match fs::symlink_metadata(&dot_git) {
        Ok(_) => bail!(
            "unregistered direct session has an independent Git authority: {}",
            session.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure!(
                path_is_missing(&dot_git)?,
                "unregistered direct session has an unreadable or dangling Git authority: {}",
                session.display()
            );
            Ok(())
        }
        Err(error) => Err(error).context(format!(
            "failed to inspect direct session Git authority {}",
            dot_git.display()
        )),
    }
}

fn push_all_session_legacies(paths: &mut Vec<PathBuf>, workspace_root: &Path) -> Result<()> {
    for session in direct_session_roots(workspace_root)? {
        push_store_legacy(paths, &session.join(STATE_DIR).join("issues"));
    }
    Ok(())
}

fn direct_session_roots(workspace_root: &Path) -> Result<Vec<PathBuf>> {
    let sessions = workspace_root.join(STATE_DIR).join(SESSIONS_DIR);
    let metadata = match fs::symlink_metadata(&sessions) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if path_is_missing(&sessions)? {
                return Ok(Vec::new());
            }
            return Err(error).context(format!(
                "failed to inspect legacy sessions {}",
                sessions.display()
            ));
        }
        Err(error) => {
            return Err(error).context(format!(
                "failed to inspect legacy sessions {}",
                sessions.display()
            ));
        }
    };
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "legacy sessions path is not a real directory: {}",
        sessions.display()
    );
    let entries = fs::read_dir(&sessions).context(format!(
        "failed to read legacy sessions {}",
        sessions.display()
    ))?;
    let mut roots = Vec::new();
    for entry in entries {
        let entry_error = format!(
            "failed to read a legacy session entry in {}",
            sessions.display()
        );
        let entry = entry.context(entry_error)?;
        let entry_path = entry.path();
        let entry_display = entry_path.display().to_string();
        let file_type_error = format!("failed to inspect legacy session entry {entry_display}");
        let file_type = entry.file_type().context(file_type_error)?;
        ensure!(
            !file_type.is_symlink(),
            "legacy session entry is a symlink and cannot be safely enumerated: {entry_display}"
        );
        if file_type.is_dir() {
            roots.push(entry_path);
        }
    }
    Ok(roots)
}

/// Discover materialized store-local legacy files below a real Git worktree
/// without walking build output. Old v2 used the raw process cwd as its root,
/// so an MCP launched below the worktree may have created this exact untracked
/// path at any depth. Both ordinary and ignored files are requested because a
/// repository may ignore nested `.usagi` directories.
fn push_materialized_git_legacies(paths: &mut Vec<PathBuf>, worktree: &Path) -> Result<()> {
    const PATHSPECS: [&str; 2] = [
        ".usagi/issues/usagi-issue-sequence/next",
        ":(glob)**/.usagi/issues/usagi-issue-sequence/next",
    ];
    let worktree_display = worktree.display().to_string();
    for ignored in [false, true] {
        let mut command = scoped_git_command(worktree);
        command.args(["ls-files", "-z", "--others"]);
        if ignored {
            command.args(["--ignored", "--exclude-standard"]);
        } else {
            command.args(["--cached", "--exclude-standard"]);
        }
        command.arg("--").args(PATHSPECS);
        let command_error = format!(
            "failed to discover materialized legacy issue authorities in {worktree_display}"
        );
        let output = command.output().context(command_error)?;
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        ensure!(
            output.status.success(),
            "failed to discover materialized legacy issue authorities in {worktree_display}: {stderr}"
        );
        for raw in output.stdout.split(|byte| *byte == 0) {
            if raw.is_empty() {
                continue;
            }
            let path_error = format!(
                "legacy issue authority path reported by Git is not UTF-8 in {worktree_display}"
            );
            let relative = std::str::from_utf8(raw).context(path_error)?;
            let relative = Path::new(relative);
            let relative_display = relative.display().to_string();
            ensure!(
                !relative.is_absolute(),
                "Git reported an absolute legacy issue authority path: {relative_display}"
            );
            let expected = Path::new(".usagi/issues/usagi-issue-sequence/next");
            if !relative.ends_with(expected) {
                // `git ls-files --others --ignored` may collapse a nested
                // registered worktree to its directory even under an exact
                // pathspec. That worktree is enumerated independently below;
                // never mistake the collapsed directory for a sequence file.
                let collapsed = worktree.join(relative);
                let collapsed_display = collapsed.display().to_string();
                ensure!(
                    collapsed.is_dir(),
                    "Git reported an unexpected legacy authority candidate: {collapsed_display}"
                );
                continue;
            }
            paths.push(worktree.join(relative));
        }
    }
    Ok(())
}

fn push_materialized_git_issue_roots(roots: &mut Vec<PathBuf>, worktree: &Path) -> Result<()> {
    const PATHSPECS: [&str; 2] = [".usagi/issues/*.md", ":(glob)**/.usagi/issues/*.md"];
    let worktree_display = worktree.display().to_string();
    for ignored in [false, true] {
        let mut command = scoped_git_command(worktree);
        command.args(["ls-files", "-z", "--others"]);
        if ignored {
            command.args(["--ignored", "--exclude-standard"]);
        } else {
            command.args(["--cached", "--exclude-standard"]);
        }
        command.arg("--").args(PATHSPECS);
        let command_error =
            format!("failed to discover materialized issue sources in {worktree_display}");
        let output = command.output().context(command_error)?;
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        ensure!(
            output.status.success(),
            "failed to discover materialized issue sources in {worktree_display}: {stderr}"
        );
        for raw in output.stdout.split(|byte| *byte == 0) {
            if raw.is_empty() {
                continue;
            }
            let path_error =
                format!("issue source path reported by Git is not UTF-8 in {worktree_display}");
            let relative = std::str::from_utf8(raw).context(path_error)?;
            let relative = Path::new(relative);
            let relative_display = relative.display().to_string();
            ensure!(
                !relative.is_absolute(),
                "Git reported an absolute issue source path: {relative_display}"
            );
            let state = relative
                .parent()
                .filter(|parent| parent.file_name().is_some_and(|name| name == "issues"))
                .and_then(Path::parent)
                .filter(|parent| parent.file_name().is_some_and(|name| name == STATE_DIR));
            if relative
                .extension()
                .and_then(|extension| extension.to_str())
                != Some("md")
                || state.is_none()
            {
                let collapsed = worktree.join(relative);
                let collapsed_display = collapsed.display().to_string();
                ensure!(
                    collapsed.is_dir(),
                    "Git reported an unexpected issue source candidate: {collapsed_display}"
                );
                continue;
            }
            let root = state.and_then(Path::parent).unwrap_or(Path::new(""));
            roots.push(normalize_path_identity(&worktree.join(root))?);
        }
    }
    Ok(())
}

fn push_materialized_git_legacies_in_all_worktrees(
    paths: &mut Vec<PathBuf>,
    repository: &Path,
) -> Result<()> {
    for worktree in git_worktree_roots(repository)? {
        push_materialized_git_legacies(paths, &worktree)?;
    }
    Ok(())
}

fn push_source_derived_git_legacies(paths: &mut Vec<PathBuf>, repository: &Path) -> Result<()> {
    let worktrees = git_worktree_roots(repository)?;
    let worktree_identities = worktrees
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let mut issue_roots = Vec::new();
    for worktree in &worktrees {
        push_materialized_git_issue_roots(&mut issue_roots, worktree)?;
    }
    issue_roots.sort();
    issue_roots.dedup();
    for root in issue_roots {
        if !worktree_identities.contains(&root) {
            // Old v2 at a registered worktree root follows `.git` to the
            // common legacy authority. At every deeper raw cwd it instead uses
            // this store-local path, which must be locked/fenced even if `next`
            // has not been created yet.
            push_store_legacy(paths, &root.join(STATE_DIR).join("issues"));
        }
    }
    Ok(())
}

fn git_worktree_roots(repository: &Path) -> Result<Vec<PathBuf>> {
    let repository_display = repository.display().to_string();
    let known_error = format!("failed to canonicalize known Git worktree {repository_display}");
    let known = fs::canonicalize(repository).context(known_error)?;
    let common_error = format!("failed to resolve Git common dir from {}", known.display());
    let common_output = scoped_git_command(&known)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .context(common_error)?;
    let common_stderr = String::from_utf8_lossy(&common_output.stderr)
        .trim()
        .to_owned();
    ensure!(
        common_output.status.success(),
        "failed to resolve Git common dir from {}: {}",
        known.display(),
        common_stderr
    );
    let common_text = std::str::from_utf8(&common_output.stdout)
        .context("Git common directory is not UTF-8")?
        .trim();
    ensure!(
        !common_text.is_empty(),
        "Git reported an empty common directory"
    );
    let common_error = format!("failed to canonicalize Git common dir {common_text}");
    let common = fs::canonicalize(common_text).context(common_error)?;

    let worktrees_error = format!("failed to enumerate Git worktrees from {repository_display}");
    let output = scoped_git_command(repository)
        .args(["worktree", "list", "--porcelain", "-z"])
        .output()
        .context(worktrees_error)?;
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    ensure!(
        output.status.success(),
        "failed to enumerate Git worktrees from {repository_display}: {stderr}"
    );
    let mut worktrees = vec![known];
    for raw in output.stdout.split(|byte| *byte == 0) {
        let Some(raw) = raw.strip_prefix(b"worktree ") else {
            continue;
        };
        let path_error =
            format!("Git worktree path is not UTF-8 while enumerating {repository_display}");
        let path = std::str::from_utf8(raw).context(path_error)?;
        ensure!(!path.is_empty(), "Git reported an empty worktree path");
        let candidate = fs::canonicalize(path)
            .context(format!("failed to canonicalize registered worktree {path}"))?;
        if candidate == common {
            // `git init --separate-git-dir` reports its common git directory
            // as the sole porcelain `worktree` entry before the first commit.
            // The validated caller worktree above is the operational root.
            continue;
        }
        validate_registered_worktree(&candidate, &common)?;
        worktrees.push(candidate);
    }
    worktrees.sort();
    worktrees.dedup();
    Ok(worktrees)
}

fn validate_conventional_workspace_repository(
    caller: &GitRepository,
    workspace_root: &Path,
) -> Result<()> {
    let workspace_identity = normalize_path_identity(workspace_root)?;
    if workspace_identity == caller.worktree_root {
        return Ok(());
    }

    let workspace = git_repository(workspace_root)?.with_context(|| {
        format!(
            "conventional workspace {} does not resolve to a Git repository",
            workspace_root.display()
        )
    })?;
    ensure!(
        workspace.common_dir == caller.common_dir,
        "caller Git repository {} has a different common directory from conventional workspace {}",
        caller.worktree_root.display(),
        workspace_root.display()
    );
    let registered = git_worktree_roots(&workspace.worktree_root)?;
    ensure!(
        registered.contains(&caller.worktree_root),
        "caller Git worktree is not registered in conventional workspace {}: {}",
        workspace_root.display(),
        caller.worktree_root.display()
    );
    Ok(())
}

fn validate_registered_worktree(candidate: &Path, expected_common: &Path) -> Result<()> {
    let candidate_display = candidate.display().to_string();
    let validation_error = format!("failed to validate registered worktree {candidate_display}");
    let validation = scoped_git_command(candidate)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .context(validation_error)?;
    ensure!(
        validation.status.success() && String::from_utf8_lossy(&validation.stdout).trim() == "true",
        "registered Git worktree is invalid: {candidate_display}"
    );
    let common_error =
        format!("failed to resolve common dir for registered worktree {candidate_display}");
    let common = scoped_git_command(candidate)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .context(common_error)?;
    ensure!(
        common.status.success(),
        "failed to resolve common dir for registered worktree {candidate_display}"
    );
    let common = std::str::from_utf8(&common.stdout)
        .context("registered worktree common directory is not UTF-8")?
        .trim();
    ensure!(
        !common.is_empty(),
        "registered worktree reported an empty common directory"
    );
    let canonical_error =
        format!("failed to canonicalize common dir for registered worktree {candidate_display}");
    let common = fs::canonicalize(common).context(canonical_error)?;
    ensure!(
        common == expected_common,
        "registered worktree belongs to a different Git common directory: {candidate_display}"
    );
    Ok(())
}

fn acquire_legacy_locks(paths: &[PathBuf]) -> Result<Vec<StoreLock>> {
    paths
        .iter()
        .map(|path| {
            let dir = path.parent().context(format!(
                "legacy issue sequence has no parent: {}",
                path.display()
            ))?;
            StoreLock::acquire(dir)
        })
        .collect()
}

fn legacy_sentinel(number: u32) -> String {
    format!("{LEGACY_V2_SENTINEL_PREFIX}{number}\n")
}

fn parse_legacy_sentinel(text: &str) -> Option<Result<u32>> {
    text.strip_prefix(LEGACY_V2_SENTINEL_PREFIX).map(|tail| {
        let number = tail
            .strip_suffix('\n')
            .context("legacy v2 migration sentinel has no canonical newline")?
            .parse::<u32>()
            .context("legacy v2 migration sentinel has an invalid floor")?;
        ensure!(
            text == legacy_sentinel(number),
            "legacy v2 migration sentinel is not canonical"
        );
        Ok(number)
    })
}

fn write_legacy_sentinel(path: &Path, number: u32) -> Result<()> {
    write_text_atomic(path, &legacy_sentinel(number))
}

/// Resolve the Git directory shared by the nearest repository worktree.
fn git_repository(start: &Path) -> Result<Option<GitRepository>> {
    let Some(dot_git) = nearest_dot_git(start)? else {
        return Ok(None);
    };
    let worktree_root = dot_git
        .parent()
        .context("a .git path must have a worktree parent")?
        .to_path_buf();
    let dot_git_display = dot_git.display().to_string();
    let metadata_error = format!("failed to inspect git directory path {dot_git_display}");
    let metadata = fs::metadata(&dot_git).context(metadata_error)?;
    ensure!(
        metadata.is_dir() || metadata.is_file(),
        "invalid git directory path {dot_git_display}"
    );
    let worktree_root_display = worktree_root.display().to_string();
    let root_error = format!("failed to canonicalize Git worktree root {worktree_root_display}");
    let worktree_root = fs::canonicalize(&worktree_root).context(root_error)?;

    // Match the production v1 resolver instead of accepting a merely existing
    // `.git` directory or gitfile target. Environment overrides are removed so
    // validation is anchored to the ancestor boundary inspected above.
    let mut command = scoped_git_command(&worktree_root);
    command.args(["rev-parse", "--path-format=absolute", "--git-common-dir"]);
    let validation_error = format!("failed to validate Git repository {worktree_root_display}");
    let output = command.output().context(validation_error)?;
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    ensure!(
        output.status.success(),
        "invalid Git repository {worktree_root_display}: {stderr}"
    );
    let reported = String::from_utf8(output.stdout).context("git common directory is not UTF-8")?;
    let reported = Path::new(reported.trim());
    ensure!(
        !reported.as_os_str().is_empty(),
        "Git reported an empty common directory for {worktree_root_display}"
    );
    let reported_display = reported.display().to_string();
    let common_error = format!("failed to canonicalize git common directory {reported_display}");
    let common_dir = fs::canonicalize(reported).context(common_error)?;
    let common_dir_display = common_dir.display().to_string();
    ensure!(
        common_dir.is_dir(),
        "git common directory is not a directory: {common_dir_display}"
    );
    Ok(Some(GitRepository {
        worktree_root,
        common_dir,
    }))
}

fn scoped_git_command(repo: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo).env("LC_ALL", "C");
    for variable in REPO_SCOPING_ENV {
        command.env_remove(variable);
    }
    command
}

/// Find the repository/worktree boundary from an arbitrary nested cwd.
fn nearest_dot_git(start: &Path) -> Result<Option<PathBuf>> {
    for ancestor in start.ancestors() {
        let dot_git = ancestor.join(".git");
        match fs::symlink_metadata(&dot_git) {
            Ok(_) => return Ok(Some(dot_git)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).context(format!("failed to inspect {}", dot_git.display()));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::persistence::json_file::{AtomicWriteStage, fail_next_atomic_write};
    use fs2::FileExt;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    const OLD_SEQUENCE_ENV: &str = "USAGI_TEST_OLD_V2_SEQUENCE";
    const OLD_READY_ENV: &str = "USAGI_TEST_OLD_V2_READY";
    const OLD_RELEASE_ENV: &str = "USAGI_TEST_OLD_V2_RELEASE";
    const OLD_V1_ROOT_ENV: &str = "USAGI_TEST_OLD_V1_ROOT";
    const OLD_V1_RESULT_ENV: &str = "USAGI_TEST_OLD_V1_RESULT";
    const OLD_V2_EMULATOR_RESULT_ENV: &str = "USAGI_TEST_OLD_V2_EMULATOR_RESULT";
    const OLD_V2_EMULATOR_RESOLVED_ENV: &str = "USAGI_TEST_OLD_V2_EMULATOR_RESOLVED";
    const OLD_V2_EMULATOR_READY_ENV: &str = "USAGI_TEST_OLD_V2_EMULATOR_READY";
    const OLD_V2_EMULATOR_RELEASE_ENV: &str = "USAGI_TEST_OLD_V2_EMULATOR_RELEASE";
    const RESOLVER_ROOT_ENV: &str = "USAGI_TEST_RESOLVER_ROOT";
    const RESOLVER_RESULT_ENV: &str = "USAGI_TEST_RESOLVER_RESULT";

    #[derive(serde::Deserialize, serde::Serialize)]
    struct OldV2EmulatorResult {
        sequence: PathBuf,
        number: u32,
    }

    fn sequence(root: &Path) -> IssueNumberSequence {
        IssueNumberSequence::new(root, root, &root.join(STATE_DIR).join("issues")).unwrap()
    }

    fn git_sequence(root: &Path) -> IssueNumberSequence {
        git(root, &["init", "-q"]);
        sequence(root)
    }

    fn start_reservation_blocked_on_legacy(
        nested: &Path,
        authority: &IssueNumberSequence,
    ) -> (thread::JoinHandle<()>, mpsc::Receiver<Result<u32>>) {
        let authority_lock = fs::File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(StoreLock::path(authority.dir()))
            .unwrap();
        let nested = nested.to_path_buf();
        let (sender, receiver) = mpsc::channel();
        let reservation = thread::spawn(move || {
            sender
                .send(
                    crate::infrastructure::store::issue::IssueStore::new(&nested)
                        .reserve_next_number(),
                )
                .unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match authority_lock.try_lock_exclusive() {
                Ok(()) => {
                    FileExt::unlock(&authority_lock).unwrap();
                    assert!(
                        Instant::now() < deadline,
                        "fixed allocator did not acquire its authority lock"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => {
                    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
                    break;
                }
            }
        }
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        (reservation, receiver)
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

    fn wait_for_emulator_release(ready_env: &str, release_env: &str, timeout_message: &str) {
        let Some(ready) = std::env::var_os(ready_env).map(PathBuf::from) else {
            return;
        };
        fs::write(&ready, b"ready\n").unwrap();
        let release = PathBuf::from(std::env::var_os(release_env).unwrap());
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            assert!(Instant::now() < deadline, "{timeout_message}");
            if release.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Resolve the allocation directory exactly as the pre-fix v2 `IssueStore`
    /// did from the raw repository argument supplied by its MCP composition.
    fn old_v2_compatibility_allocation_dir(raw_cwd: &Path) -> Result<PathBuf> {
        let dot_git = raw_cwd.join(".git");
        if dot_git.is_dir() {
            return Ok(dot_git.join(LEGACY_V2_DIR));
        }
        if !dot_git.exists() {
            return Ok(raw_cwd.join(STATE_DIR).join("issues").join(LEGACY_V2_DIR));
        }

        let git_dir = old_v2_compatibility_git_dir_from_dot_git(&dot_git)?;
        let common_dir_file = git_dir.join("commondir");
        let common_dir = match fs::read_to_string(&common_dir_file) {
            Ok(text) => {
                let path = Path::new(text.trim());
                if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    git_dir.join(path)
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => git_dir,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to read old-v2 git common directory {}",
                        common_dir_file.display()
                    )
                });
            }
        };
        Ok(common_dir.join(LEGACY_V2_DIR))
    }

    fn old_v2_compatibility_git_dir_from_dot_git(dot_git: &Path) -> Result<PathBuf> {
        let text = fs::read_to_string(dot_git).with_context(|| {
            format!(
                "failed to read old-v2 git directory file {}",
                dot_git.display()
            )
        })?;
        let path = text
            .strip_prefix("gitdir: ")
            .or_else(|| text.strip_prefix("gitdir:\t"))
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .with_context(|| format!("invalid old-v2 git directory file {}", dot_git.display()))?;
        let path = Path::new(path);
        Ok(if path.is_absolute() {
            path.to_path_buf()
        } else {
            dot_git.with_file_name(path)
        })
    }

    fn old_v2_compatibility_max_number(raw_cwd: &Path) -> Result<u32> {
        let issues = raw_cwd.join(STATE_DIR).join("issues");
        let entries = match fs::read_dir(&issues) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read old-v2 store {}", issues.display()));
            }
        };
        let mut maximum = 0;
        for entry in entries {
            let path = entry
                .context(format!("failed to read old-v2 store {}", issues.display()))?
                .path();
            if path.extension().and_then(|extension| extension.to_str()) == Some("md") {
                maximum = maximum.max(
                    crate::infrastructure::store::issue::number_from_filename(&path).unwrap_or(0),
                );
            }
        }
        Ok(maximum)
    }

    fn old_v2_compatibility_reserved(sequence: &Path) -> Result<u32> {
        match fs::read_to_string(sequence) {
            Ok(text) => text.trim().parse::<u32>().with_context(|| {
                format!("invalid old-v2 issue sequence in {}", sequence.display())
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(error) => Err(error).with_context(|| {
                format!(
                    "failed to read old-v2 issue sequence {}",
                    sequence.display()
                )
            }),
        }
    }

    #[test]
    fn old_v2_compatibility_emulator_resolver_matches_pre_fix_layouts() {
        let tmp = tempfile::tempdir().unwrap();

        let direct = tmp.path().join("direct");
        fs::create_dir_all(direct.join(".git")).unwrap();
        assert_eq!(
            old_v2_compatibility_allocation_dir(&direct).unwrap(),
            direct.join(".git").join(LEGACY_V2_DIR)
        );

        let nested = tmp.path().join("nested");
        fs::create_dir(&nested).unwrap();
        assert_eq!(
            old_v2_compatibility_allocation_dir(&nested).unwrap(),
            nested.join(STATE_DIR).join("issues").join(LEGACY_V2_DIR)
        );

        let worktree = tmp.path().join("worktree");
        let private = tmp.path().join("private");
        let common = tmp.path().join("common");
        fs::create_dir(&worktree).unwrap();
        fs::create_dir(&private).unwrap();
        fs::create_dir(&common).unwrap();
        fs::write(worktree.join(".git"), b"gitdir: ../private\n").unwrap();
        assert_eq!(
            old_v2_compatibility_allocation_dir(&worktree).unwrap(),
            worktree.join("../private").join(LEGACY_V2_DIR)
        );

        fs::write(private.join("commondir"), b"../common\n").unwrap();
        assert_eq!(
            old_v2_compatibility_allocation_dir(&worktree).unwrap(),
            worktree
                .join("../private")
                .join("../common")
                .join(LEGACY_V2_DIR)
        );

        fs::write(
            private.join("commondir"),
            common.to_string_lossy().as_bytes(),
        )
        .unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir:\t{}\n", private.display()),
        )
        .unwrap();
        assert_eq!(
            old_v2_compatibility_allocation_dir(&worktree).unwrap(),
            common.join(LEGACY_V2_DIR)
        );

        fs::write(worktree.join(".git"), b"gitdir: \n").unwrap();
        assert!(old_v2_compatibility_allocation_dir(&worktree).is_err());
        assert!(
            old_v2_compatibility_git_dir_from_dot_git(&worktree.join("missing-gitfile")).is_err()
        );
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", private.display()),
        )
        .unwrap();
        fs::remove_file(private.join("commondir")).unwrap();
        fs::create_dir(private.join("commondir")).unwrap();
        assert!(old_v2_compatibility_allocation_dir(&worktree).is_err());

        let sequence = tmp.path().join("old-v2-next");
        assert_eq!(old_v2_compatibility_reserved(&sequence).unwrap(), 0);
        fs::write(&sequence, b"7\n").unwrap();
        assert_eq!(old_v2_compatibility_reserved(&sequence).unwrap(), 7);
        fs::write(&sequence, b"not-a-number\n").unwrap();
        assert!(old_v2_compatibility_reserved(&sequence).is_err());
        fs::remove_file(&sequence).unwrap();
        fs::create_dir(&sequence).unwrap();
        assert!(old_v2_compatibility_reserved(&sequence).is_err());

        let source_root = tmp.path().join("old-v2-sources");
        assert_eq!(old_v2_compatibility_max_number(&source_root).unwrap(), 0);
        let issues = source_root.join(STATE_DIR).join("issues");
        fs::create_dir_all(&issues).unwrap();
        fs::write(issues.join("012-high.md"), b"ignored body\n").unwrap();
        fs::write(issues.join("prefixless.md"), b"ignored body\n").unwrap();
        fs::write(issues.join("README.txt"), b"ignored body\n").unwrap();
        assert_eq!(old_v2_compatibility_max_number(&source_root).unwrap(), 12);

        let broken_root = tmp.path().join("broken-old-v2-sources");
        fs::create_dir_all(broken_root.join(STATE_DIR)).unwrap();
        fs::write(
            broken_root.join(STATE_DIR).join("issues"),
            b"not a directory\n",
        )
        .unwrap();
        assert!(old_v2_compatibility_max_number(&broken_root).is_err());
    }

    fn only_legacy(authority: &IssueNumberSequence) -> PathBuf {
        let paths = authority.legacy_v2_sequences().unwrap();
        assert_eq!(paths.len(), 1);
        paths.into_iter().next().unwrap()
    }

    fn observed_legacy_path(authority: &IssueNumberSequence, expected: &Path) -> PathBuf {
        let expected_identity = normalize_path_identity(expected.parent().unwrap())
            .unwrap()
            .join(expected.file_name().unwrap());
        authority
            .legacy_v2_sequences()
            .unwrap()
            .into_iter()
            .find(|candidate| {
                normalize_path_identity(candidate.parent().unwrap()).is_ok_and(|parent| {
                    parent.join(candidate.file_name().unwrap()) == expected_identity
                })
            })
            .unwrap()
    }

    fn seed_sequence(authority: &IssueNumberSequence, last_reserved: u32) {
        json_file::write_atomic(
            authority.dir(),
            &authority.sequence_path(),
            &SequenceFile {
                version: SEQUENCE_VERSION,
                last_reserved,
                migration_floor: None,
            },
        )
        .unwrap();
    }

    fn seed_migration_blocker(authority: &IssueNumberSequence, floor: u32) {
        json_file::write_atomic(
            authority.dir(),
            &authority.sequence_path(),
            &SequenceFile {
                version: SEQUENCE_VERSION,
                last_reserved: u32::MAX,
                migration_floor: Some(floor),
            },
        )
        .unwrap();
    }

    fn v1_reserve(authority: &IssueNumberSequence) -> Result<u32> {
        #[derive(Deserialize, Serialize)]
        struct V1Sequence {
            last_reserved: u32,
        }

        let _lock = StoreLock::acquire(authority.dir())?;
        let sequence = match fs::read_to_string(authority.sequence_path()) {
            Ok(text) => serde_json::from_str::<V1Sequence>(&text)
                .ok()
                .map_or(0, |sequence| sequence.last_reserved),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error.into()),
        };
        let reservations = authority.reservations_dir();
        let mut journal = 0;
        match fs::read_dir(&reservations) {
            Ok(entries) => {
                for entry in entries {
                    let path = entry?.path();
                    let Some(number) = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .and_then(|name| name.strip_suffix(RESERVATION_SUFFIX))
                        .and_then(|number| number.parse::<u32>().ok())
                    else {
                        continue;
                    };
                    journal = journal.max(number);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let number = sequence
            .max(journal)
            .checked_add(1)
            .context("issue number space is exhausted")?;
        fs::create_dir_all(&reservations)?;
        write_text_atomic(
            &reservations.join(reservation_name(number)),
            &format!("{number}\n"),
        )?;
        json_file::write_versioned(
            authority.dir(),
            &authority.sequence_path(),
            &V1Sequence {
                last_reserved: number,
            },
        )?;
        Ok(number)
    }

    fn seed_reservation(authority: &IssueNumberSequence, number: u32) {
        fs::create_dir_all(authority.reservations_dir()).unwrap();
        fs::write(
            authority.reservations_dir().join(reservation_name(number)),
            format!("{number}\n"),
        )
        .unwrap();
    }

    #[test]
    fn v1_sequence_preseed_allocates_the_following_number() {
        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        seed_sequence(&authority, 515);

        assert_eq!(authority.reserve(|| Ok(0)).unwrap(), 516);
        let persisted: SequenceFile = json_file::read(&authority.sequence_path())
            .unwrap()
            .unwrap();
        assert_eq!(persisted.version, 1);
        assert_eq!(persisted.last_reserved, 516);
        assert_eq!(persisted.migration_floor, None);
        assert_eq!(
            fs::read_to_string(authority.reservations_dir().join("0000000516.reserved")).unwrap(),
            "516\n"
        );
    }

    #[test]
    fn v1_emulator_propagates_journal_and_atomic_write_failures() {
        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        fs::create_dir_all(authority.reservations_dir()).unwrap();
        fs::write(authority.reservations_dir().join("README"), b"ignored\n").unwrap();
        assert_eq!(v1_reserve(&authority).unwrap(), 1);

        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        fs::create_dir_all(authority.sequence_path()).unwrap();
        assert!(v1_reserve(&authority).is_err());

        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        seed_sequence(&authority, 1);
        fs::write(authority.reservations_dir(), b"not a directory\n").unwrap();
        assert!(v1_reserve(&authority).is_err());

        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        let marker = authority.reservations_dir().join(reservation_name(1));
        fail_next_atomic_write(&marker, AtomicWriteStage::Write);
        assert!(v1_reserve(&authority).is_err());
        assert!(!marker.exists());
        assert!(!authority.sequence_path().exists());

        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        fail_next_atomic_write(&authority.sequence_path(), AtomicWriteStage::Write);
        assert!(v1_reserve(&authority).is_err());
        assert!(
            authority
                .reservations_dir()
                .join(reservation_name(1))
                .exists()
        );
        assert!(!authority.sequence_path().exists());
    }

    #[test]
    fn legacy_v2_high_water_is_folded_and_durably_fenced() {
        let tmp = tempfile::tempdir().unwrap();
        let authority = git_sequence(tmp.path());
        seed_sequence(&authority, 515);
        let legacy = only_legacy(&authority);
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, "700\n").unwrap();

        assert_eq!(authority.reserve(|| Ok(650)).unwrap(), 701);
        assert_eq!(fs::read_to_string(&legacy).unwrap(), legacy_sentinel(701));
        assert!(
            fs::read_to_string(&legacy)
                .unwrap()
                .trim()
                .parse::<u32>()
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(authority.legacy_v2_migration_path()).unwrap(),
            "701\n"
        );
        assert_eq!(authority.reserve(|| Ok(0)).unwrap(), 702);
        assert_eq!(fs::read_to_string(&legacy).unwrap(), legacy_sentinel(701));
    }

    #[test]
    fn completed_migration_remains_bidirectionally_compatible_with_v1() {
        let tmp = tempfile::tempdir().unwrap();
        let authority = git_sequence(tmp.path());
        let legacy = only_legacy(&authority);
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, "515\n").unwrap();

        assert_eq!(authority.reserve(|| Ok(0)).unwrap(), 516);
        let fence = fs::read(&legacy).unwrap();
        let migration = fs::read(authority.legacy_v2_migration_path()).unwrap();

        assert_eq!(v1_reserve(&authority).unwrap(), 517);
        assert_eq!(fs::read(&legacy).unwrap(), fence);
        assert_eq!(
            fs::read(authority.legacy_v2_migration_path()).unwrap(),
            migration
        );
        assert!(
            authority
                .reservations_dir()
                .join("0000000517.reserved")
                .is_file()
        );
        assert_eq!(authority.reserve(|| Ok(0)).unwrap(), 518);
        assert_eq!(
            authority.read_sequence().unwrap(),
            SequenceState::Normal(518)
        );
    }

    #[test]
    fn migration_marker_requires_the_matching_legacy_fence() {
        for contents in [
            "900\n".to_string(),
            "corrupt\n".to_string(),
            legacy_sentinel(700),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let authority = git_sequence(tmp.path());
            let legacy = only_legacy(&authority);
            fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            fs::write(&legacy, contents).unwrap();
            fs::create_dir_all(authority.dir()).unwrap();
            fs::write(authority.legacy_v2_migration_path(), "701\n").unwrap();

            assert!(authority.reserve(|| Ok(999)).is_err());
            assert!(!authority.reservations_dir().exists());
            assert!(!authority.sequence_path().exists());
        }
    }

    #[test]
    fn migration_fails_when_both_live_allocators_trail_a_fenced_floor() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git(root, &["init", "-q"]);
        let nested = root.join("crates/core");
        let local_store = nested.join(".usagi/issues");
        let authority = IssueNumberSequence::new(&nested, root, &local_store).unwrap();
        let shared = root.join(".git/usagi-issue-sequence/next");
        let local = legacy_sequence_for_store(&local_store);
        for path in [&shared, &local] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        fs::write(&shared, legacy_sentinel(700)).unwrap();
        fs::write(&local, "100\n").unwrap();
        seed_sequence(&authority, 500);
        fs::write(authority.legacy_v2_migration_path(), "700\n").unwrap();
        let sequence_before = fs::read(authority.sequence_path()).unwrap();
        let shared_before = fs::read(&shared).unwrap();
        let local_before = fs::read(&local).unwrap();

        let error = authority.reserve(|| Ok(0)).unwrap_err();
        assert!(error.to_string().contains("neither live legacy v2 nor v1"));
        assert_eq!(
            fs::read(authority.sequence_path()).unwrap(),
            sequence_before
        );
        assert_eq!(fs::read(&shared).unwrap(), shared_before);
        assert_eq!(fs::read(&local).unwrap(), local_before);
        assert!(!authority.reservations_dir().exists());

        // If the v1 blocker was already durable, v1 is no longer live. The
        // fixed allocator can safely fence the stale legacy path and recover.
        seed_migration_blocker(&authority, 700);
        assert_eq!(authority.reserve(|| Ok(0)).unwrap(), 701);
        assert_eq!(fs::read_to_string(&shared).unwrap(), legacy_sentinel(701));
        assert_eq!(fs::read_to_string(&local).unwrap(), legacy_sentinel(701));
    }

    #[test]
    fn fixed_only_source_floor_blocks_v1_before_fencing_legacy() {
        for stage in [AtomicWriteStage::Write, AtomicWriteStage::Rename] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path();
            git(root, &["init", "-q"]);
            let nested = root.join("crates/core");
            let local_store = nested.join(".usagi/issues");
            let authority = IssueNumberSequence::new(&nested, root, &local_store).unwrap();
            let shared = root.join(".git/usagi-issue-sequence/next");
            let local = legacy_sequence_for_store(&local_store);
            for path in [&shared, &local] {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
            }
            fs::write(&shared, legacy_sentinel(500)).unwrap();
            fs::write(&local, "800\n").unwrap();
            seed_sequence(&authority, 500);
            fs::write(authority.legacy_v2_migration_path(), "500\n").unwrap();
            let local_write_path = observed_legacy_path(&authority, &local);
            fail_next_atomic_write(&local_write_path, stage);

            let error = authority
                .reserve_observing_floors(
                    || {
                        Ok(ExistingIssueFloors {
                            all: 800,
                            v1_visible: 0,
                        })
                    },
                    || {},
                )
                .unwrap_err();
            assert!(
                error.to_string().contains("injected atomic"),
                "unexpected migration error: {error:#}"
            );
            assert_eq!(
                authority.read_sequence().unwrap(),
                SequenceState::MigrationBlocked(800)
            );
            assert_eq!(fs::read_to_string(&local).unwrap(), "800\n");

            let result = root.join("blocked-old-v1-result");
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["old_v1_emulator_process_helper", "--nocapture"])
                .env(OLD_V1_ROOT_ENV, root)
                .env(OLD_V1_RESULT_ENV, &result)
                .output()
                .unwrap();
            assert!(!output.status.success());
            assert!(!result.exists());

            let output = Command::new(std::env::current_exe().unwrap())
                .args(["old_v2_sequence_emulator_process_helper", "--nocapture"])
                .env(OLD_SEQUENCE_ENV, &local)
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(fs::read_to_string(&local).unwrap(), "801\n");

            assert_eq!(
                authority
                    .reserve_observing_floors(
                        || {
                            Ok(ExistingIssueFloors {
                                all: 800,
                                v1_visible: 0,
                            })
                        },
                        || {},
                    )
                    .unwrap(),
                802
            );
        }
    }

    #[test]
    fn every_migration_crash_boundary_advances_without_reusing_a_floor() {
        for boundary in 0..5 {
            let tmp = tempfile::tempdir().unwrap();
            let authority = git_sequence(tmp.path());
            let legacy = only_legacy(&authority);
            fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            if boundary >= 1 {
                fs::write(&legacy, legacy_sentinel(701)).unwrap();
            } else {
                fs::write(&legacy, "700\n").unwrap();
            }
            seed_migration_blocker(&authority, 700);
            if boundary >= 2 {
                seed_reservation(&authority, 701);
            }
            if boundary >= 3 {
                fs::write(authority.legacy_v2_migration_path(), "701\n").unwrap();
            }
            if boundary >= 4 {
                seed_sequence(&authority, 701);
            }

            let expected = if boundary == 0 { 701 } else { 702 };
            assert_eq!(
                authority.reserve(|| Ok(0)).unwrap(),
                expected,
                "boundary {boundary}"
            );
            let expected_fence = if boundary == 4 { 701 } else { expected };
            assert_eq!(
                fs::read_to_string(&legacy).unwrap(),
                legacy_sentinel(expected_fence)
            );
            assert_eq!(
                fs::read_to_string(authority.legacy_v2_migration_path()).unwrap(),
                format!("{expected_fence}\n")
            );
            assert_eq!(
                authority.read_sequence().unwrap(),
                SequenceState::Normal(expected)
            );
        }
    }

    #[test]
    fn sentinel_write_failure_keeps_v1_blocked_and_folds_later_old_v2_progress() {
        for stage in [AtomicWriteStage::Write, AtomicWriteStage::Rename] {
            let tmp = tempfile::tempdir().unwrap();
            let authority = git_sequence(tmp.path());
            let legacy = only_legacy(&authority);
            fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            fs::write(&legacy, "700\n").unwrap();
            fail_next_atomic_write(&legacy, stage);

            assert!(authority.reserve(|| Ok(0)).is_err());
            assert_eq!(fs::read_to_string(&legacy).unwrap(), "700\n");
            assert_eq!(
                authority.read_sequence().unwrap(),
                SequenceState::MigrationBlocked(700)
            );
            assert!(!authority.reservations_dir().exists());
            assert!(!authority.legacy_v2_migration_path().exists());
            let blocker_before = fs::read(authority.sequence_path()).unwrap();
            let legacy_before = fs::read(&legacy).unwrap();
            assert!(v1_reserve(&authority).is_err());
            assert_eq!(fs::read(authority.sequence_path()).unwrap(), blocker_before);
            assert_eq!(fs::read(&legacy).unwrap(), legacy_before);
            assert!(!authority.reservations_dir().exists());

            let output = Command::new(std::env::current_exe().unwrap())
                .args(["old_v2_sequence_emulator_process_helper", "--nocapture"])
                .env(OLD_SEQUENCE_ENV, &legacy)
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(fs::read_to_string(&legacy).unwrap(), "701\n");

            assert_eq!(authority.reserve(|| Ok(0)).unwrap(), 702);
            assert_eq!(fs::read_to_string(&legacy).unwrap(), legacy_sentinel(702));
            assert_eq!(
                authority.read_sequence().unwrap(),
                SequenceState::Normal(702)
            );
        }
    }

    #[test]
    fn migration_marker_write_failure_consumes_the_reservation_and_retry_advances() {
        for stage in [AtomicWriteStage::Write, AtomicWriteStage::Rename] {
            let tmp = tempfile::tempdir().unwrap();
            let authority = git_sequence(tmp.path());
            let legacy = only_legacy(&authority);
            fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            fs::write(&legacy, "700\n").unwrap();
            fail_next_atomic_write(&authority.legacy_v2_migration_path(), stage);

            assert!(authority.reserve(|| Ok(0)).is_err());
            assert_eq!(fs::read_to_string(&legacy).unwrap(), legacy_sentinel(701));
            assert!(
                authority
                    .reservations_dir()
                    .join("0000000701.reserved")
                    .is_file()
            );
            assert_eq!(
                authority.read_sequence().unwrap(),
                SequenceState::MigrationBlocked(700)
            );
            assert!(!authority.legacy_v2_migration_path().exists());

            assert_eq!(authority.reserve(|| Ok(0)).unwrap(), 702);
            assert_eq!(fs::read_to_string(&legacy).unwrap(), legacy_sentinel(702));
        }
    }

    #[test]
    fn migration_reservation_failure_keeps_both_old_allocators_blocked() {
        for stage in [AtomicWriteStage::Write, AtomicWriteStage::Rename] {
            let tmp = tempfile::tempdir().unwrap();
            let authority = git_sequence(tmp.path());
            let legacy = only_legacy(&authority);
            fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            fs::write(&legacy, "515\n").unwrap();
            let marker = authority.reservations_dir().join("0000000516.reserved");
            fs::create_dir_all(authority.reservations_dir()).unwrap();
            fail_next_atomic_write(&marker, stage);

            assert!(authority.reserve(|| Ok(0)).is_err());
            assert_eq!(
                authority.read_sequence().unwrap(),
                SequenceState::MigrationBlocked(515)
            );
            assert_eq!(fs::read_to_string(&legacy).unwrap(), legacy_sentinel(516));
            assert!(!marker.exists());
            assert!(v1_reserve(&authority).is_err());

            assert_eq!(authority.reserve(|| Ok(0)).unwrap(), 517);
            assert_eq!(fs::read_to_string(&legacy).unwrap(), legacy_sentinel(517));
        }
    }

    #[test]
    fn migration_sequence_failure_leaves_the_marker_and_blocker_as_journal() {
        for stage in [AtomicWriteStage::Write, AtomicWriteStage::Rename] {
            let tmp = tempfile::tempdir().unwrap();
            let authority = git_sequence(tmp.path());
            let legacy = only_legacy(&authority);
            fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            fs::write(&legacy, "515\n").unwrap();
            let sequence_path = authority.sequence_path();

            assert!(
                authority
                    .reserve_observing(|| Ok(0), || fail_next_atomic_write(&sequence_path, stage),)
                    .is_err()
            );
            assert_eq!(
                authority.read_sequence().unwrap(),
                SequenceState::MigrationBlocked(515)
            );
            assert_eq!(fs::read_to_string(&legacy).unwrap(), legacy_sentinel(516));
            assert!(
                authority
                    .reservations_dir()
                    .join("0000000516.reserved")
                    .is_file()
            );
            assert_eq!(
                fs::read_to_string(authority.legacy_v2_migration_path()).unwrap(),
                "516\n"
            );
            assert!(v1_reserve(&authority).is_err());

            assert_eq!(authority.reserve(|| Ok(0)).unwrap(), 517);
            assert_eq!(
                authority.read_sequence().unwrap(),
                SequenceState::Normal(517)
            );
        }
    }

    #[test]
    fn non_git_migration_folds_every_existing_root_and_session_legacy_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let sessions = root.join(".usagi/sessions");
        let first_store = sessions.join("first/.usagi/issues");
        let second_store = sessions.join("second/.usagi/issues");
        fs::create_dir_all(&first_store).unwrap();
        fs::create_dir_all(&second_store).unwrap();
        let first_legacy = legacy_sequence_for_store(&first_store);
        fs::create_dir_all(first_legacy.parent().unwrap()).unwrap();
        fs::write(&first_legacy, legacy_sentinel(400)).unwrap();
        let root_legacy = legacy_sequence_for_store(&root.join(".usagi/issues"));
        fs::create_dir_all(root_legacy.parent().unwrap()).unwrap();
        fs::write(&root_legacy, legacy_sentinel(400)).unwrap();
        let second_legacy = legacy_sequence_for_store(&second_store);
        fs::create_dir_all(second_legacy.parent().unwrap()).unwrap();
        fs::write(&second_legacy, "515\n").unwrap();
        let authority =
            IssueNumberSequence::new(&sessions.join("first"), root, &first_store).unwrap();

        assert_eq!(authority.reserve(|| Ok(0)).unwrap(), 516);
        for path in authority.legacy_v2_sequences().unwrap() {
            assert!(matches!(
                IssueNumberSequence::read_legacy_v2_sequence(&path).unwrap(),
                LegacyState::Fenced(_)
            ));
        }
        assert_eq!(
            fs::read_to_string(&root_legacy).unwrap(),
            legacy_sentinel(516)
        );
        assert!(!authority.legacy_v2_migration_path().exists());

        let third_store = sessions.join("third/.usagi/issues");
        let third_legacy = legacy_sequence_for_store(&third_store);
        fs::create_dir_all(third_legacy.parent().unwrap()).unwrap();
        fs::write(&third_legacy, "700\n").unwrap();
        assert_eq!(authority.reserve(|| Ok(0)).unwrap(), 701);
        assert_eq!(
            fs::read_to_string(third_legacy).unwrap(),
            legacy_sentinel(701)
        );
    }

    #[test]
    fn non_git_legacy_enumeration_skips_files_and_has_no_shared_sequence() {
        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        let sessions = tmp.path().join(STATE_DIR).join(SESSIONS_DIR);
        fs::create_dir_all(&sessions).unwrap();
        fs::write(sessions.join("README"), b"not a session\n").unwrap();

        let paths = authority.legacy_v2_sequences().unwrap();
        assert!(authority.shared_legacy_sequence().is_none());
        assert!(authority.registered_worktrees().unwrap().is_empty());
        assert!(authority.materialized_git_issue_roots().unwrap().is_empty());
        assert!(
            paths
                .iter()
                .all(|path| !path.starts_with(sessions.join("README")))
        );

        let unreadable_root = tempfile::tempdir().unwrap();
        let sessions = unreadable_root.path().join(STATE_DIR).join(SESSIONS_DIR);
        fs::create_dir_all(sessions.parent().unwrap()).unwrap();
        fs::write(&sessions, b"not a directory\n").unwrap();
        assert!(push_all_session_legacies(&mut Vec::new(), unreadable_root.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn session_discovery_io_errors_are_propagated_without_materialization() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        fn assert_permission_denied(error: &anyhow::Error) {
            assert_eq!(
                error
                    .root_cause()
                    .downcast_ref::<std::io::Error>()
                    .unwrap()
                    .kind(),
                std::io::ErrorKind::PermissionDenied
            );
        }

        let dangling_git_tmp = tempfile::tempdir().unwrap();
        let dangling_ancestor = dangling_git_tmp.path().join("dangling");
        symlink("missing-session-parent", &dangling_ancestor).unwrap();
        let dangling_git_session = dangling_ancestor.join("plain");
        let error = ensure_no_independent_git_authority(&dangling_git_session).unwrap_err();
        assert!(error.to_string().contains("dangling Git authority"));
        assert_eq!(
            fs::read_link(&dangling_ancestor).unwrap(),
            PathBuf::from("missing-session-parent")
        );

        let unreadable_git_tmp = tempfile::tempdir().unwrap();
        let unreadable_git_session = unreadable_git_tmp.path().join("plain");
        fs::create_dir(&unreadable_git_session).unwrap();
        let original = fs::metadata(&unreadable_git_session).unwrap().permissions();
        fs::set_permissions(&unreadable_git_session, fs::Permissions::from_mode(0o000)).unwrap();
        let result = ensure_no_independent_git_authority(&unreadable_git_session);
        fs::set_permissions(&unreadable_git_session, original).unwrap();
        let error = result.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("failed to inspect direct session Git authority")
        );
        assert_permission_denied(&error);

        let dangling_sessions_tmp = tempfile::tempdir().unwrap();
        let dangling_sessions_root = dangling_sessions_tmp.path().join("workspace");
        symlink("missing-workspace", &dangling_sessions_root).unwrap();
        let error = direct_session_roots(&dangling_sessions_root).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("failed to inspect legacy sessions")
        );
        assert_eq!(
            fs::read_link(&dangling_sessions_root).unwrap(),
            PathBuf::from("missing-workspace")
        );

        let unreadable_parent_tmp = tempfile::tempdir().unwrap();
        let unreadable_parent_root = unreadable_parent_tmp.path();
        let state = unreadable_parent_root.join(STATE_DIR);
        fs::create_dir(&state).unwrap();
        let original = fs::metadata(&state).unwrap().permissions();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o000)).unwrap();
        let result = direct_session_roots(unreadable_parent_root);
        fs::set_permissions(&state, original).unwrap();
        let error = result.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("failed to inspect legacy sessions")
        );
        assert_permission_denied(&error);

        let unreadable_sessions_tmp = tempfile::tempdir().unwrap();
        let unreadable_sessions_root = unreadable_sessions_tmp.path();
        let sessions = unreadable_sessions_root.join(STATE_DIR).join(SESSIONS_DIR);
        fs::create_dir_all(&sessions).unwrap();
        let original = fs::metadata(&sessions).unwrap().permissions();
        fs::set_permissions(&sessions, fs::Permissions::from_mode(0o000)).unwrap();
        let result = direct_session_roots(unreadable_sessions_root);
        fs::set_permissions(&sessions, original).unwrap();
        let error = result.unwrap_err();
        assert!(error.to_string().contains("failed to read legacy sessions"));
        assert_permission_denied(&error);
    }

    #[cfg(unix)]
    #[test]
    fn every_git_legacy_discovery_boundary_propagates_before_materialization() {
        use std::os::unix::fs::symlink;

        fn scoped(
            root: &Path,
            workspace_root: PathBuf,
            worktree_root: PathBuf,
        ) -> IssueNumberSequence {
            IssueNumberSequence {
                dir: root.join("authority"),
                worktree_root: worktree_root.clone(),
                legacy_scope: LegacyScope::Git {
                    shared_sequence: root.join("common/usagi-issue-sequence/next"),
                    workspace_root,
                    worktree_root,
                    current_is_nested: false,
                    current_issue_store: root.join("current/.usagi/issues"),
                },
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let worktree = tmp.path().join("worktree");
        fs::create_dir_all(worktree.join(STATE_DIR).join("issues")).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        fs::write(
            worktree.join(STATE_DIR).join("issues").join(LEGACY_V2_DIR),
            b"not a directory\n",
        )
        .unwrap();
        assert!(
            scoped(tmp.path(), workspace, worktree)
                .legacy_v2_sequences()
                .is_err()
        );
        assert!(!tmp.path().join("authority").exists());

        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let worktree = tmp.path().join("worktree");
        fs::create_dir_all(workspace.join(STATE_DIR).join("issues")).unwrap();
        fs::create_dir_all(&worktree).unwrap();
        fs::write(
            workspace.join(STATE_DIR).join("issues").join(LEGACY_V2_DIR),
            b"not a directory\n",
        )
        .unwrap();
        assert!(
            scoped(tmp.path(), workspace, worktree)
                .legacy_v2_sequences()
                .is_err()
        );
        assert!(!tmp.path().join("authority").exists());

        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let worktree = tmp.path().join("worktree");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(worktree.join(STATE_DIR)).unwrap();
        symlink(
            "missing-sessions",
            worktree.join(STATE_DIR).join(SESSIONS_DIR),
        )
        .unwrap();
        assert!(
            scoped(tmp.path(), workspace, worktree)
                .legacy_v2_sequences()
                .is_err()
        );
        assert!(!tmp.path().join("authority").exists());

        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let worktree = tmp.path().join("worktree");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&worktree).unwrap();
        assert!(
            scoped(tmp.path(), workspace, worktree)
                .legacy_v2_sequences()
                .is_err()
        );
        assert!(!tmp.path().join("authority").exists());
    }

    #[test]
    fn multiple_unfenced_git_authorities_fail_before_any_authoritative_write() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git(root, &["init", "-q"]);
        let nested = root.join("crates/core");
        let local_store = nested.join(".usagi/issues");
        let authority = IssueNumberSequence::new(&nested, root, &local_store).unwrap();
        let common = root.join(".git/usagi-issue-sequence/next");
        let local = legacy_sequence_for_store(&local_store);
        for (path, contents) in [(&common, "500\n"), (&local, "515\n")] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        let common_before = fs::read(&common).unwrap();
        let local_before = fs::read(&local).unwrap();

        let error = authority.reserve(|| Ok(450)).unwrap_err();
        assert!(error.to_string().contains("multiple independent legacy"));
        assert_eq!(fs::read(&common).unwrap(), common_before);
        assert_eq!(fs::read(&local).unwrap(), local_before);
        assert!(!authority.sequence_path().exists());
        assert!(!authority.reservations_dir().exists());
        assert!(!authority.legacy_v2_migration_path().exists());
    }

    #[test]
    fn multiple_unfenced_non_git_sessions_fail_without_changing_any_floor() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let sessions = root.join(".usagi/sessions");
        let first_store = sessions.join("first/.usagi/issues");
        let second_store = sessions.join("second/.usagi/issues");
        let first = legacy_sequence_for_store(&first_store);
        let second = legacy_sequence_for_store(&second_store);
        for (path, contents) in [(&first, "500\n"), (&second, "515\n")] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        let authority =
            IssueNumberSequence::new(&sessions.join("first"), root, &first_store).unwrap();
        let first_before = fs::read(&first).unwrap();
        let second_before = fs::read(&second).unwrap();

        let error = authority.reserve(|| Ok(450)).unwrap_err();
        assert!(error.to_string().contains("reconcile all durable floors"));
        assert_eq!(fs::read(&first).unwrap(), first_before);
        assert_eq!(fs::read(&second).unwrap(), second_before);
        assert!(!authority.sequence_path().exists());
        assert!(!authority.reservations_dir().exists());
        assert!(!authority.legacy_v2_migration_path().exists());
    }

    #[test]
    fn multiple_known_missing_non_git_paths_are_unfenced_and_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let first_root = root.join(".usagi/sessions/first");
        let second_root = root.join(".usagi/sessions/second");
        fs::create_dir_all(&first_root).unwrap();
        fs::create_dir_all(&second_root).unwrap();
        let first_store = first_root.join(".usagi/issues");
        let authority = IssueNumberSequence::new(&first_root, root, &first_store).unwrap();
        let legacy_paths = authority.legacy_v2_sequences().unwrap();
        assert_eq!(legacy_paths.len(), 3);
        assert!(legacy_paths.iter().all(|path| !path.exists()));

        let error = authority.reserve(|| Ok(0)).unwrap_err();
        assert!(error.to_string().contains("multiple independent legacy"));
        assert!(legacy_paths.iter().all(|path| !path.exists()));
        assert!(!authority.sequence_path().exists());
        assert!(!authority.reservations_dir().exists());
    }

    #[test]
    fn partial_sentinel_failure_stays_blocked_and_retry_advances() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git(root, &["init", "-q"]);
        let root_authority = sequence(root);
        assert_eq!(root_authority.reserve(|| Ok(499)).unwrap(), 500);
        let common = only_legacy(&root_authority);
        assert_eq!(fs::read_to_string(&common).unwrap(), legacy_sentinel(500));

        let nested = root.join("crates/core");
        let local_store = nested.join(".usagi/issues");
        let local = legacy_sequence_for_store(&local_store);
        fs::create_dir_all(local.parent().unwrap()).unwrap();
        fs::write(&local, "515\n").unwrap();
        let authority = IssueNumberSequence::new(&nested, root, &local_store).unwrap();
        fail_next_atomic_write(&common, AtomicWriteStage::Rename);

        assert!(authority.reserve(|| Ok(0)).is_err());
        assert_eq!(
            authority.read_sequence().unwrap(),
            SequenceState::MigrationBlocked(515)
        );
        assert_eq!(fs::read_to_string(&local).unwrap(), legacy_sentinel(516));
        assert_eq!(fs::read_to_string(&common).unwrap(), legacy_sentinel(500));

        assert_eq!(authority.reserve(|| Ok(0)).unwrap(), 517);
        assert_eq!(fs::read_to_string(&local).unwrap(), legacy_sentinel(517));
        assert_eq!(fs::read_to_string(&common).unwrap(), legacy_sentinel(517));
        assert_eq!(
            fs::read_to_string(authority.legacy_v2_migration_path()).unwrap(),
            "517\n"
        );
    }

    #[test]
    fn blocked_remigration_recovers_an_old_marker_after_marker_update_failure() {
        for stage in [AtomicWriteStage::Write, AtomicWriteStage::Rename] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path();
            git(root, &["init", "-q"]);
            let root_authority = sequence(root);
            assert_eq!(root_authority.reserve(|| Ok(499)).unwrap(), 500);
            let common = only_legacy(&root_authority);

            let nested = root.join("crates/core");
            let local_store = nested.join(".usagi/issues");
            let local = legacy_sequence_for_store(&local_store);
            fs::create_dir_all(local.parent().unwrap()).unwrap();
            fs::write(&local, "515\n").unwrap();
            let authority = IssueNumberSequence::new(&nested, root, &local_store).unwrap();
            fail_next_atomic_write(&authority.legacy_v2_migration_path(), stage);

            assert!(authority.reserve(|| Ok(0)).is_err());
            assert_eq!(
                authority.read_sequence().unwrap(),
                SequenceState::MigrationBlocked(515)
            );
            assert_eq!(fs::read_to_string(&common).unwrap(), legacy_sentinel(516));
            assert_eq!(fs::read_to_string(&local).unwrap(), legacy_sentinel(516));
            assert_eq!(
                fs::read_to_string(authority.legacy_v2_migration_path()).unwrap(),
                "500\n"
            );

            assert_eq!(authority.reserve(|| Ok(0)).unwrap(), 517);
            assert_eq!(fs::read_to_string(&common).unwrap(), legacy_sentinel(517));
            assert_eq!(fs::read_to_string(&local).unwrap(), legacy_sentinel(517));
            assert_eq!(
                fs::read_to_string(authority.legacy_v2_migration_path()).unwrap(),
                "517\n"
            );
        }
    }

    #[test]
    fn abandoned_marker_survives_a_missing_sequence_and_is_never_reused() {
        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        assert_eq!(authority.reserve(|| Ok(0)).unwrap(), 1);
        fs::remove_file(authority.sequence_path()).unwrap();
        assert_eq!(authority.reserve(|| Ok(0)).unwrap(), 2);
    }

    #[test]
    fn sequence_commit_failure_leaves_the_marker_as_a_crash_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        assert_eq!(authority.reserve(|| Ok(0)).unwrap(), 1);
        fail_next_atomic_write(&authority.sequence_path(), AtomicWriteStage::Rename);

        assert!(authority.reserve(|| Ok(0)).is_err());
        assert!(
            authority
                .reservations_dir()
                .join("0000000002.reserved")
                .is_file()
        );
        assert_eq!(authority.reserve(|| Ok(0)).unwrap(), 3);
    }

    #[test]
    fn blocker_write_failure_preserves_both_usable_authorities() {
        for stage in [AtomicWriteStage::Write, AtomicWriteStage::Rename] {
            let tmp = tempfile::tempdir().unwrap();
            let authority = git_sequence(tmp.path());
            let legacy = only_legacy(&authority);
            fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            fs::write(&legacy, "515\n").unwrap();
            seed_sequence(&authority, 500);
            let sequence_before = fs::read(authority.sequence_path()).unwrap();
            let legacy_before = fs::read(&legacy).unwrap();
            fail_next_atomic_write(&authority.sequence_path(), stage);

            assert!(authority.reserve(|| Ok(0)).is_err());
            assert_eq!(
                fs::read(authority.sequence_path()).unwrap(),
                sequence_before
            );
            assert_eq!(fs::read(&legacy).unwrap(), legacy_before);
            assert!(!authority.reservations_dir().exists());
            assert!(!authority.legacy_v2_migration_path().exists());
        }
    }

    #[test]
    fn v1_leading_migration_fences_old_v2_before_a_blocker_failure() {
        for stage in [AtomicWriteStage::Write, AtomicWriteStage::Rename] {
            let tmp = tempfile::tempdir().unwrap();
            let authority = git_sequence(tmp.path());
            let legacy = only_legacy(&authority);
            fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            fs::write(&legacy, "500\n").unwrap();
            seed_sequence(&authority, 800);
            let sequence_before = fs::read(authority.sequence_path()).unwrap();
            fail_next_atomic_write(&authority.sequence_path(), stage);

            assert!(authority.reserve(|| Ok(0)).is_err());
            assert_eq!(
                fs::read(authority.sequence_path()).unwrap(),
                sequence_before
            );
            assert_eq!(fs::read_to_string(&legacy).unwrap(), legacy_sentinel(800));
            assert!(!authority.reservations_dir().exists());
            assert!(!authority.legacy_v2_migration_path().exists());

            let output = Command::new(std::env::current_exe().unwrap())
                .args(["old_v2_sequence_emulator_process_helper", "--nocapture"])
                .env(OLD_SEQUENCE_ENV, &legacy)
                .output()
                .unwrap();
            assert!(!output.status.success());

            // The compatible v1 side is the only old allocator still able to
            // progress after this first-boundary failure. A retry folds it.
            assert_eq!(v1_reserve(&authority).unwrap(), 801);
            assert_eq!(authority.reserve(|| Ok(0)).unwrap(), 802);
            assert_eq!(fs::read_to_string(&legacy).unwrap(), legacy_sentinel(802));
        }
    }

    #[test]
    fn corrupt_authority_state_fails_without_a_new_reservation() {
        for sequence_contents in [
            "not json\n",
            r#"{"version":2,"last_reserved":8}"#,
            r#"{"version":1,"last_reserved":4294967295,"migration_floor":4294967295}"#,
            r#"{"version":1,"last_reserved":8,"migration_floor":7}"#,
            r#"{"version":1,"last_reserved":8,"unknown":7}"#,
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let authority = git_sequence(tmp.path());
            let legacy = only_legacy(&authority);
            fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            fs::write(&legacy, "515\n").unwrap();
            let legacy_before = fs::read(&legacy).unwrap();
            fs::create_dir_all(authority.dir()).unwrap();
            fs::write(authority.sequence_path(), sequence_contents).unwrap();
            let sequence_before = fs::read(authority.sequence_path()).unwrap();
            assert!(authority.reserve(|| Ok(99)).is_err());
            assert!(!authority.reservations_dir().exists());
            assert!(!authority.legacy_v2_migration_path().exists());
            assert_eq!(
                fs::read(authority.sequence_path()).unwrap(),
                sequence_before
            );
            assert_eq!(fs::read(&legacy).unwrap(), legacy_before);
        }

        for legacy_contents in ["not a number\n", "migrated-to-usagi-issue-numbers:x\n"] {
            let tmp = tempfile::tempdir().unwrap();
            let authority = git_sequence(tmp.path());
            let legacy = only_legacy(&authority);
            fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            fs::write(&legacy, legacy_contents).unwrap();
            assert!(authority.reserve(|| Ok(99)).is_err());
            assert!(!authority.reservations_dir().exists());
            assert!(!authority.sequence_path().exists());
        }

        for migration_contents in ["not a number\n", "7"] {
            let tmp = tempfile::tempdir().unwrap();
            let authority = git_sequence(tmp.path());
            fs::create_dir_all(authority.dir()).unwrap();
            fs::write(authority.legacy_v2_migration_path(), migration_contents).unwrap();
            assert!(authority.reserve(|| Ok(99)).is_err());
            assert!(!authority.reservations_dir().exists());
            assert!(!authority.sequence_path().exists());
        }
    }

    #[test]
    fn unreadable_authority_paths_fail_without_a_new_reservation() {
        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        fs::create_dir_all(authority.sequence_path()).unwrap();
        assert!(authority.reserve(|| Ok(99)).is_err());
        assert!(!authority.reservations_dir().exists());

        let tmp = tempfile::tempdir().unwrap();
        let authority = git_sequence(tmp.path());
        let legacy = only_legacy(&authority);
        fs::create_dir_all(&legacy).unwrap();
        assert!(authority.reserve(|| Ok(99)).is_err());
        assert!(!authority.reservations_dir().exists());
        assert!(!authority.sequence_path().exists());

        let tmp = tempfile::tempdir().unwrap();
        let authority = git_sequence(tmp.path());
        fs::create_dir_all(authority.legacy_v2_migration_path()).unwrap();
        assert!(authority.reserve(|| Ok(99)).is_err());
        assert!(!authority.reservations_dir().exists());
        assert!(!authority.sequence_path().exists());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_authority_symlinks_fail_closed_without_being_replaced() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        fs::create_dir_all(authority.dir()).unwrap();
        symlink("missing-sequence", authority.sequence_path()).unwrap();
        assert!(authority.reserve(|| Ok(99)).is_err());
        assert!(
            fs::symlink_metadata(authority.sequence_path())
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!authority.reservations_dir().exists());

        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        let legacy = tmp.path().join(".usagi/issues/usagi-issue-sequence/next");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        symlink("missing-legacy", &legacy).unwrap();
        assert!(authority.reserve(|| Ok(99)).is_err());
        assert!(
            fs::symlink_metadata(&legacy)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!authority.sequence_path().exists());
        assert!(!authority.reservations_dir().exists());

        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        fs::create_dir_all(authority.dir()).unwrap();
        symlink("missing-migration", authority.legacy_v2_migration_path()).unwrap();
        assert!(authority.reserve(|| Ok(99)).is_err());
        assert!(
            fs::symlink_metadata(authority.legacy_v2_migration_path())
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!authority.sequence_path().exists());
        assert!(!authority.reservations_dir().exists());

        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        fs::create_dir_all(authority.dir()).unwrap();
        symlink("missing-reservations", authority.reservations_dir()).unwrap();
        assert!(authority.reserve(|| Ok(99)).is_err());
        assert!(
            fs::symlink_metadata(authority.reservations_dir())
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!authority.sequence_path().exists());
    }

    #[test]
    fn corrupt_or_unreadable_markers_fail_closed_while_non_markers_are_ignored() {
        for (name, contents) in [
            ("not-a-number.reserved", "1\n"),
            ("12.reserved", "12\n"),
            ("0000000012.reserved", "13\n"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let authority = sequence(tmp.path());
            fs::create_dir_all(authority.reservations_dir()).unwrap();
            fs::write(authority.reservations_dir().join("README"), "ignored").unwrap();
            fs::write(authority.reservations_dir().join(name), contents).unwrap();
            assert!(authority.reserve(|| Ok(99)).is_err(), "accepted {name}");
            assert!(!authority.sequence_path().exists());
        }

        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        fs::create_dir_all(authority.reservations_dir().join("0000000012.reserved")).unwrap();
        assert!(authority.reserve(|| Ok(99)).is_err());
        assert!(!authority.sequence_path().exists());

        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        fs::create_dir_all(authority.dir()).unwrap();
        fs::write(authority.reservations_dir(), "not a directory").unwrap();
        assert!(authority.reserve(|| Ok(99)).is_err());
        assert!(!authority.sequence_path().exists());
    }

    #[test]
    fn path_validation_errors_do_not_materialize_authority_state() {
        let tmp = tempfile::tempdir().unwrap();
        let overlong = tmp.path().join("x".repeat(4096));

        assert!(!path_is_missing(Path::new("")).unwrap());
        assert!(normalize_path_identity(Path::new("")).is_err());
        assert!(deduplicate_legacy_paths(vec![PathBuf::new()]).is_err());
        assert!(deduplicate_legacy_paths(vec![PathBuf::from("legacy/..")]).is_err());
        assert!(acquire_legacy_locks(&[PathBuf::new()]).is_err());

        assert!(ensure_authority_absent(&overlong).is_err());
        assert!(path_is_missing(&overlong).is_err());
        assert!(normalize_path_identity(&overlong).is_err());
        assert!(nearest_dot_git(&overlong).is_err());
        assert!(push_existing_store_legacy(&mut Vec::new(), &overlong).is_err());
        assert!(!tmp.path().join(STATE_DIR).exists());
    }

    #[test]
    fn reservation_and_git_discovery_io_failures_are_propagated() {
        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        fs::create_dir_all(authority.dir()).unwrap();
        fs::write(authority.reservations_dir(), b"not a directory\n").unwrap();
        assert!(authority.write_reservation_marker(1).is_err());
        assert!(!authority.sequence_path().exists());

        let non_repository = tempfile::tempdir().unwrap();
        assert!(push_materialized_git_legacies(&mut Vec::new(), non_repository.path()).is_err());
        assert!(push_materialized_git_issue_roots(&mut Vec::new(), non_repository.path()).is_err());
        assert!(git_worktree_roots(non_repository.path()).is_err());
        assert!(
            validate_registered_worktree(non_repository.path(), non_repository.path()).is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_directory_dot_git_is_rejected_without_creating_an_authority() {
        use std::os::unix::net::UnixListener;

        let tmp = tempfile::tempdir().unwrap();
        let dot_git = tmp.path().join(".git");
        let _socket = UnixListener::bind(&dot_git).unwrap();

        let error = git_repository(tmp.path())
            .err()
            .expect("non-directory .git was accepted");
        assert!(error.to_string().contains("invalid git directory path"));
        assert!(!tmp.path().join(STATE_DIR).exists());
    }

    #[test]
    fn reservation_marker_failure_before_commit_is_retryable() {
        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        let marker = authority.reservations_dir().join("0000000001.reserved");
        let legacy_paths = authority.legacy_v2_sequences().unwrap();
        assert_eq!(legacy_paths.len(), 1);
        fs::create_dir_all(legacy_paths[0].parent().unwrap()).unwrap();
        fs::write(&legacy_paths[0], legacy_sentinel(0)).unwrap();
        fs::create_dir_all(authority.reservations_dir()).unwrap();
        fail_next_atomic_write(&marker, AtomicWriteStage::Write);

        assert!(authority.reserve(|| Ok(0)).is_err());
        assert!(!authority.sequence_path().exists());
        assert_eq!(
            fs::read_to_string(&legacy_paths[0]).unwrap(),
            legacy_sentinel(0)
        );
        assert_eq!(authority.reserve(|| Ok(0)).unwrap(), 1);
    }

    #[test]
    fn number_space_exhaustion_is_reported_after_compatible_allocators_are_fenced() {
        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        let error = authority.reserve(|| Ok(u32::MAX)).unwrap_err();
        assert!(error.to_string().contains("u32 range is exhausted"));
        assert!(!authority.reservations_dir().exists());
        assert_eq!(
            authority.read_sequence().unwrap(),
            SequenceState::Normal(u32::MAX)
        );
        let legacy = only_legacy(&authority);
        assert_eq!(
            fs::read_to_string(legacy).unwrap(),
            legacy_sentinel(u32::MAX)
        );
        assert!(!authority.legacy_v2_migration_path().exists());

        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        seed_sequence(&authority, u32::MAX - 1);
        assert_eq!(authority.reserve(|| Ok(0)).unwrap(), u32::MAX);
        assert_eq!(
            authority.read_sequence().unwrap(),
            SequenceState::Normal(u32::MAX)
        );
        let sequence_before = fs::read(authority.sequence_path()).unwrap();
        let marker_before = fs::read(
            authority
                .reservations_dir()
                .join(reservation_name(u32::MAX)),
        )
        .unwrap();
        assert!(authority.reserve(|| Ok(0)).is_err());
        assert_eq!(
            fs::read(authority.sequence_path()).unwrap(),
            sequence_before
        );
        assert_eq!(
            fs::read(
                authority
                    .reservations_dir()
                    .join(reservation_name(u32::MAX))
            )
            .unwrap(),
            marker_before
        );

        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        seed_sequence(&authority, u32::MAX - 1);
        assert_eq!(v1_reserve(&authority).unwrap(), u32::MAX);
        assert!(authority.reserve(|| Ok(0)).is_err());

        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        assert!(authority.write_migration_blocker(u32::MAX).is_err());
        assert!(!authority.sequence_path().exists());
    }

    #[test]
    fn exhausted_git_sequence_durably_fences_an_old_v2_emulator_process() {
        let tmp = tempfile::tempdir().unwrap();
        let authority = git_sequence(tmp.path());
        let legacy = only_legacy(&authority);
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, b"515\n").unwrap();
        seed_sequence(&authority, u32::MAX);

        let error = authority.reserve(|| Ok(0)).unwrap_err();
        assert!(error.to_string().contains("u32 range is exhausted"));
        assert_eq!(
            authority.read_sequence().unwrap(),
            SequenceState::Normal(u32::MAX)
        );
        assert_eq!(
            fs::read_to_string(&legacy).unwrap(),
            legacy_sentinel(u32::MAX)
        );
        assert_eq!(
            fs::read_to_string(authority.legacy_v2_migration_path()).unwrap(),
            format!("{}\n", u32::MAX)
        );
        assert!(!authority.reservations_dir().exists());

        let sequence_before = fs::read(authority.sequence_path()).unwrap();
        let migration_before = fs::read(authority.legacy_v2_migration_path()).unwrap();
        let legacy_before = fs::read(&legacy).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["old_v2_sequence_emulator_process_helper", "--nocapture"])
            .env(OLD_SEQUENCE_ENV, &legacy)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(
            fs::read(authority.sequence_path()).unwrap(),
            sequence_before
        );
        assert_eq!(
            fs::read(authority.legacy_v2_migration_path()).unwrap(),
            migration_before
        );
        assert_eq!(fs::read(&legacy).unwrap(), legacy_before);
    }

    #[test]
    fn source_only_exhaustion_requires_a_live_allocator_to_see_the_floor() {
        let tmp = tempfile::tempdir().unwrap();
        let authority = git_sequence(tmp.path());
        let legacy = only_legacy(&authority);
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, b"600\n").unwrap();
        seed_sequence(&authority, 500);
        let sequence_before = fs::read(authority.sequence_path()).unwrap();
        let legacy_before = fs::read(&legacy).unwrap();

        let error = authority
            .reserve_observing_floors(
                || {
                    Ok(ExistingIssueFloors {
                        all: u32::MAX,
                        v1_visible: 0,
                    })
                },
                thread::yield_now,
            )
            .unwrap_err();
        assert!(error.to_string().contains("neither live legacy v2 nor v1"));
        assert_eq!(
            fs::read(authority.sequence_path()).unwrap(),
            sequence_before
        );
        assert_eq!(fs::read(&legacy).unwrap(), legacy_before);
        assert!(!authority.legacy_v2_migration_path().exists());
        assert!(!authority.reservations_dir().exists());

        let tmp = tempfile::tempdir().unwrap();
        let authority = git_sequence(tmp.path());
        let legacy = only_legacy(&authority);
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, legacy_sentinel(500)).unwrap();
        seed_sequence(&authority, 500);
        fs::write(authority.legacy_v2_migration_path(), b"500\n").unwrap();

        let error = authority
            .reserve_observing_floors(
                || {
                    Ok(ExistingIssueFloors {
                        all: u32::MAX,
                        v1_visible: 0,
                    })
                },
                thread::yield_now,
            )
            .unwrap_err();
        assert!(error.to_string().contains("u32 range is exhausted"));
        assert_eq!(
            authority.read_sequence().unwrap(),
            SequenceState::Normal(u32::MAX)
        );
        assert!(matches!(
            IssueNumberSequence::read_legacy_v2_sequence(&legacy).unwrap(),
            LegacyState::Fenced(_)
        ));
        assert!(authority.read_legacy_v2_migration().unwrap().is_some());
        assert!(!authority.reservations_dir().exists());

        let legacy_before = fs::read(&legacy).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["old_v2_sequence_emulator_process_helper", "--nocapture"])
            .env(OLD_SEQUENCE_ENV, &legacy)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(fs::read(&legacy).unwrap(), legacy_before);
    }

    #[test]
    fn exhausted_v1_leading_sentinel_failures_recover_without_a_reservation() {
        for stage in [AtomicWriteStage::Write, AtomicWriteStage::Rename] {
            let tmp = tempfile::tempdir().unwrap();
            let authority = git_sequence(tmp.path());
            let legacy = only_legacy(&authority);
            fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            fs::write(&legacy, b"515\n").unwrap();
            seed_sequence(&authority, u32::MAX);
            let sequence_before = fs::read(authority.sequence_path()).unwrap();
            let legacy_before = fs::read(&legacy).unwrap();
            fail_next_atomic_write(&legacy, stage);

            let error = authority.reserve(|| Ok(0)).unwrap_err();
            assert!(error.to_string().contains("injected atomic"));
            assert_eq!(
                fs::read(authority.sequence_path()).unwrap(),
                sequence_before
            );
            assert_eq!(fs::read(&legacy).unwrap(), legacy_before);
            assert!(!authority.legacy_v2_migration_path().exists());
            assert!(!authority.reservations_dir().exists());
            assert!(v1_reserve(&authority).is_err());
            assert_eq!(
                fs::read(authority.sequence_path()).unwrap(),
                sequence_before
            );

            let error = authority.reserve(|| Ok(0)).unwrap_err();
            assert!(error.to_string().contains("u32 range is exhausted"));
            assert_eq!(
                authority.read_sequence().unwrap(),
                SequenceState::Normal(u32::MAX)
            );
            assert_eq!(
                fs::read_to_string(&legacy).unwrap(),
                legacy_sentinel(u32::MAX)
            );
            assert_eq!(
                fs::read_to_string(authority.legacy_v2_migration_path()).unwrap(),
                format!("{}\n", u32::MAX)
            );
            assert!(!authority.reservations_dir().exists());
            assert!(v1_reserve(&authority).is_err());

            let legacy_before = fs::read(&legacy).unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["old_v2_sequence_emulator_process_helper", "--nocapture"])
                .env(OLD_SEQUENCE_ENV, &legacy)
                .output()
                .unwrap();
            assert!(!output.status.success());
            assert_eq!(fs::read(&legacy).unwrap(), legacy_before);
        }
    }

    #[test]
    fn exhausted_legacy_leading_sequence_failures_recover_without_a_reservation() {
        for stage in [AtomicWriteStage::Write, AtomicWriteStage::Rename] {
            let tmp = tempfile::tempdir().unwrap();
            let authority = git_sequence(tmp.path());
            let legacy = only_legacy(&authority);
            fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            fs::write(&legacy, format!("{}\n", u32::MAX)).unwrap();
            seed_sequence(&authority, 500);
            let sequence_before = fs::read(authority.sequence_path()).unwrap();
            let legacy_before = fs::read(&legacy).unwrap();
            fail_next_atomic_write(&authority.sequence_path(), stage);

            let error = authority.reserve(|| Ok(0)).unwrap_err();
            assert!(error.to_string().contains("injected atomic"));
            assert_eq!(
                fs::read(authority.sequence_path()).unwrap(),
                sequence_before
            );
            assert_eq!(fs::read(&legacy).unwrap(), legacy_before);
            assert!(!authority.legacy_v2_migration_path().exists());
            assert!(!authority.reservations_dir().exists());

            let output = Command::new(std::env::current_exe().unwrap())
                .args(["old_v2_sequence_emulator_process_helper", "--nocapture"])
                .env(OLD_SEQUENCE_ENV, &legacy)
                .output()
                .unwrap();
            assert!(!output.status.success());
            assert_eq!(fs::read(&legacy).unwrap(), legacy_before);

            let error = authority.reserve(|| Ok(0)).unwrap_err();
            assert!(error.to_string().contains("u32 range is exhausted"));
            assert_eq!(
                authority.read_sequence().unwrap(),
                SequenceState::Normal(u32::MAX)
            );
            assert_eq!(
                fs::read_to_string(&legacy).unwrap(),
                legacy_sentinel(u32::MAX)
            );
            assert_eq!(
                fs::read_to_string(authority.legacy_v2_migration_path()).unwrap(),
                format!("{}\n", u32::MAX)
            );
            assert!(!authority.reservations_dir().exists());
            assert!(v1_reserve(&authority).is_err());
        }
    }

    #[test]
    fn exhausted_marker_failures_recover_from_terminal_sequence_and_sentinel() {
        for stage in [AtomicWriteStage::Write, AtomicWriteStage::Rename] {
            let tmp = tempfile::tempdir().unwrap();
            let authority = git_sequence(tmp.path());
            let legacy = only_legacy(&authority);
            fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            fs::write(&legacy, legacy_sentinel(500)).unwrap();
            seed_sequence(&authority, 500);
            fs::write(authority.legacy_v2_migration_path(), b"500\n").unwrap();
            fail_next_atomic_write(&authority.legacy_v2_migration_path(), stage);

            let error = authority
                .reserve_observing_floors(
                    || {
                        Ok(ExistingIssueFloors {
                            all: u32::MAX,
                            v1_visible: 0,
                        })
                    },
                    thread::yield_now,
                )
                .unwrap_err();
            assert!(error.to_string().contains("injected atomic"));
            assert_eq!(
                authority.read_sequence().unwrap(),
                SequenceState::Normal(u32::MAX)
            );
            assert_eq!(
                fs::read_to_string(&legacy).unwrap(),
                legacy_sentinel(u32::MAX)
            );
            assert_eq!(
                fs::read_to_string(authority.legacy_v2_migration_path()).unwrap(),
                "500\n"
            );
            assert!(!authority.reservations_dir().exists());
            assert!(v1_reserve(&authority).is_err());

            let legacy_before = fs::read(&legacy).unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["old_v2_sequence_emulator_process_helper", "--nocapture"])
                .env(OLD_SEQUENCE_ENV, &legacy)
                .output()
                .unwrap();
            assert!(!output.status.success());
            assert_eq!(fs::read(&legacy).unwrap(), legacy_before);

            let error = authority
                .reserve_observing_floors(
                    || {
                        Ok(ExistingIssueFloors {
                            all: u32::MAX,
                            v1_visible: 0,
                        })
                    },
                    thread::yield_now,
                )
                .unwrap_err();
            assert!(error.to_string().contains("u32 range is exhausted"));
            assert_eq!(
                authority.read_sequence().unwrap(),
                SequenceState::Normal(u32::MAX)
            );
            assert_eq!(
                fs::read_to_string(&legacy).unwrap(),
                legacy_sentinel(u32::MAX)
            );
            assert_eq!(
                fs::read_to_string(authority.legacy_v2_migration_path()).unwrap(),
                format!("{}\n", u32::MAX)
            );
            assert!(!authority.reservations_dir().exists());
        }
    }

    #[test]
    fn exhausted_multi_legacy_sentinel_failures_converge_without_a_reservation() {
        for stage in [AtomicWriteStage::Write, AtomicWriteStage::Rename] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path();
            git(root, &["init", "-q"]);
            let nested = root.join("crates/core");
            let local_store = nested.join(STATE_DIR).join("issues");
            let authority = IssueNumberSequence::new(&nested, root, &local_store).unwrap();
            let shared = authority.shared_legacy_sequence().unwrap().to_path_buf();
            let local = legacy_sequence_for_store(&local_store);
            for path in [&shared, &local] {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
            }
            fs::write(&shared, legacy_sentinel(500)).unwrap();
            fs::write(&local, format!("{}\n", u32::MAX)).unwrap();
            seed_sequence(&authority, 500);
            seed_reservation(&authority, 500);
            fs::write(authority.legacy_v2_migration_path(), b"500\n").unwrap();

            let local_write_path = observed_legacy_path(&authority, &local);
            let legacy_paths = authority.legacy_v2_sequences().unwrap();
            assert_eq!(legacy_paths, vec![shared.clone(), local_write_path.clone()]);
            let reservation = authority.reservations_dir().join(reservation_name(500));
            let reservation_before = fs::read(&reservation).unwrap();
            fail_next_atomic_write(&local_write_path, stage);

            let error = authority.reserve(|| Ok(0)).unwrap_err();
            assert!(error.to_string().contains("injected atomic"));
            assert_eq!(
                authority.read_sequence().unwrap(),
                SequenceState::Normal(u32::MAX)
            );
            assert_eq!(
                fs::read_to_string(&shared).unwrap(),
                legacy_sentinel(u32::MAX)
            );
            assert_eq!(
                fs::read_to_string(&local).unwrap(),
                format!("{}\n", u32::MAX)
            );
            assert_eq!(
                fs::read_to_string(authority.legacy_v2_migration_path()).unwrap(),
                "500\n"
            );
            assert_eq!(fs::read(&reservation).unwrap(), reservation_before);
            assert_eq!(
                fs::read_dir(authority.reservations_dir()).unwrap().count(),
                1
            );

            let error = authority.reserve(|| Ok(0)).unwrap_err();
            assert!(error.to_string().contains("u32 range is exhausted"));
            assert_eq!(
                authority.read_sequence().unwrap(),
                SequenceState::Normal(u32::MAX)
            );
            for path in [&shared, &local] {
                assert_eq!(fs::read_to_string(path).unwrap(), legacy_sentinel(u32::MAX));
            }
            assert_eq!(
                fs::read_to_string(authority.legacy_v2_migration_path()).unwrap(),
                format!("{}\n", u32::MAX)
            );
            assert_eq!(fs::read(&reservation).unwrap(), reservation_before);
            assert_eq!(
                fs::read_dir(authority.reservations_dir()).unwrap().count(),
                1
            );
        }
    }

    #[test]
    fn exhausted_final_sequence_failures_recover_from_the_blocked_journal_state() {
        for stage in [AtomicWriteStage::Write, AtomicWriteStage::Rename] {
            let tmp = tempfile::tempdir().unwrap();
            let authority = git_sequence(tmp.path());
            let legacy = only_legacy(&authority);
            fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            fs::write(&legacy, legacy_sentinel(u32::MAX)).unwrap();
            seed_migration_blocker(&authority, u32::MAX - 1);
            seed_reservation(&authority, u32::MAX);
            fs::write(
                authority.legacy_v2_migration_path(),
                format!("{}\n", u32::MAX),
            )
            .unwrap();
            let blocker_before = fs::read(authority.sequence_path()).unwrap();
            let legacy_before = fs::read(&legacy).unwrap();
            let marker_before = fs::read(authority.legacy_v2_migration_path()).unwrap();
            let reservation = authority
                .reservations_dir()
                .join(reservation_name(u32::MAX));
            let reservation_before = fs::read(&reservation).unwrap();
            fail_next_atomic_write(&authority.sequence_path(), stage);

            let error = authority.reserve(|| Ok(0)).unwrap_err();
            assert!(error.to_string().contains("injected atomic"));
            assert_eq!(fs::read(authority.sequence_path()).unwrap(), blocker_before);
            assert_eq!(fs::read(&legacy).unwrap(), legacy_before);
            assert_eq!(
                fs::read(authority.legacy_v2_migration_path()).unwrap(),
                marker_before
            );
            assert_eq!(fs::read(&reservation).unwrap(), reservation_before);
            assert_eq!(
                fs::read_dir(authority.reservations_dir()).unwrap().count(),
                1
            );
            assert!(v1_reserve(&authority).is_err());

            let output = Command::new(std::env::current_exe().unwrap())
                .args(["old_v2_sequence_emulator_process_helper", "--nocapture"])
                .env(OLD_SEQUENCE_ENV, &legacy)
                .output()
                .unwrap();
            assert!(!output.status.success());
            assert_eq!(fs::read(&legacy).unwrap(), legacy_before);

            let error = authority.reserve(|| Ok(0)).unwrap_err();
            assert!(error.to_string().contains("u32 range is exhausted"));
            assert_eq!(
                authority.read_sequence().unwrap(),
                SequenceState::Normal(u32::MAX)
            );
            assert_eq!(
                fs::read_to_string(&legacy).unwrap(),
                legacy_sentinel(u32::MAX)
            );
            assert_eq!(
                fs::read_to_string(authority.legacy_v2_migration_path()).unwrap(),
                format!("{}\n", u32::MAX)
            );
            assert_eq!(fs::read(&reservation).unwrap(), reservation_before);
            assert_eq!(
                fs::read_dir(authority.reservations_dir()).unwrap().count(),
                1
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn legacy_lock_order_is_identical_across_symlink_aliases() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        fs::create_dir_all(real.join("a")).unwrap();
        fs::create_dir_all(real.join("b")).unwrap();
        let alias = tmp.path().join("alias");
        symlink(&real, &alias).unwrap();

        let left = deduplicate_legacy_paths(vec![
            alias.join("b/next"),
            real.join("a/next"),
            alias.join("a/next"),
        ])
        .unwrap();
        let right = deduplicate_legacy_paths(vec![
            real.join("b/next"),
            alias.join("a/next"),
            real.join("a/next"),
        ])
        .unwrap();
        assert_eq!(left, right);
        let real = fs::canonicalize(real).unwrap();
        assert_eq!(left, vec![real.join("a/next"), real.join("b/next")]);
    }

    #[test]
    fn git_and_linked_worktree_layouts_share_the_v1_authority() {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q"]);
        let main = sequence(tmp.path());
        assert_eq!(
            main.dir(),
            fs::canonicalize(tmp.path().join(".git"))
                .unwrap()
                .join("usagi/issue-numbers")
        );

        git(tmp.path(), &["config", "user.email", "test@example.com"]);
        git(tmp.path(), &["config", "user.name", "Test"]);
        fs::write(tmp.path().join("README.md"), "workspace\n").unwrap();
        git(tmp.path(), &["add", "README.md"]);
        git(tmp.path(), &["commit", "-q", "-m", "init"]);
        let linked_root = tmp.path().join(".usagi/sessions/linked-layout");
        fs::create_dir_all(linked_root.parent().unwrap()).unwrap();
        git(
            tmp.path(),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "test-linked-layout",
                linked_root.to_str().unwrap(),
            ],
        );
        let linked = sequence(&linked_root);
        assert_eq!(
            linked.dir(),
            fs::canonicalize(tmp.path().join(".git"))
                .unwrap()
                .join("usagi/issue-numbers")
        );
    }

    #[test]
    fn registered_session_worktree_nested_caller_uses_the_workspace_authority() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "user.name", "Test"]);
        fs::write(root.join("README.md"), "workspace\n").unwrap();
        git(&root, &["add", "README.md"]);
        git(&root, &["commit", "-q", "-m", "init"]);

        let authority = sequence(&root);
        assert_eq!(authority.reserve(|| Ok(514)).unwrap(), 515);
        let linked = root.join(".usagi/sessions/registered");
        fs::create_dir_all(linked.parent().unwrap()).unwrap();
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "test-registered-session-caller",
                linked.to_str().unwrap(),
            ],
        );
        let nested = linked.join("crates/core");
        fs::create_dir_all(&nested).unwrap();

        assert_eq!(
            crate::infrastructure::store::issue::IssueStore::new(&nested)
                .reserve_next_number()
                .unwrap(),
            516
        );
        let caller =
            IssueNumberSequence::new(&nested, &root, &nested.join(".usagi/issues")).unwrap();
        assert_eq!(caller.dir(), authority.dir());
        assert_eq!(caller.read_sequence().unwrap(), SequenceState::Normal(516));
        assert!(
            caller
                .reservations_dir()
                .join(reservation_name(516))
                .is_file()
        );
        assert!(!linked.join(".git/usagi").exists());
        assert!(!nested.join(".usagi/issue-numbers").exists());
    }

    #[test]
    fn independent_repo_below_conventional_sessions_is_rejected_without_mutation() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q"]);
        let authority = sequence(&root);
        seed_sequence(&authority, 515);
        seed_reservation(&authority, 515);
        let sequence_before = fs::read(authority.sequence_path()).unwrap();
        let reservation = authority.reservations_dir().join(reservation_name(515));
        let reservation_before = fs::read(&reservation).unwrap();

        let rogue = root.join(".usagi/sessions/rogue");
        let nested = rogue.join("crates/core");
        fs::create_dir_all(&nested).unwrap();
        git(&rogue, &["init", "-q"]);
        let error = crate::infrastructure::store::issue::IssueStore::new(&nested)
            .reserve_next_number()
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("different common directory from conventional workspace")
        );
        assert_eq!(
            fs::read(authority.sequence_path()).unwrap(),
            sequence_before
        );
        assert_eq!(fs::read(&reservation).unwrap(), reservation_before);
        assert_eq!(
            fs::read_dir(authority.reservations_dir()).unwrap().count(),
            1
        );
        assert!(!authority.legacy_v2_migration_path().exists());
        assert!(!rogue.join(".git/usagi").exists());
        assert!(!rogue.join(".git/usagi-issue-sequence").exists());
        assert!(!nested.join(".usagi").exists());
    }

    #[test]
    fn conventional_workspace_validation_rejects_missing_git_and_unregistered_caller() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        let non_git = tmp.path().join("non-git");
        let unregistered = tmp.path().join("unregistered");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&non_git).unwrap();
        fs::create_dir(&unregistered).unwrap();
        git(&root, &["init", "-q"]);
        let caller = git_repository(&root).unwrap().unwrap();

        let error = validate_conventional_workspace_repository(&caller, &non_git).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not resolve to a Git repository")
        );

        let unregistered_caller = GitRepository {
            worktree_root: fs::canonicalize(&unregistered).unwrap(),
            common_dir: caller.common_dir.clone(),
        };
        let error =
            validate_conventional_workspace_repository(&unregistered_caller, &root).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("caller Git worktree is not registered")
        );
        assert!(!root.join(".git/usagi").exists());
        assert!(!non_git.join(".usagi").exists());
        assert!(!unregistered.join(".usagi").exists());
    }

    #[test]
    fn git_transition_rejects_an_existing_non_git_authority_without_reusing_its_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cached_non_git = sequence(root);
        assert_eq!(cached_non_git.reserve(|| Ok(0)).unwrap(), 1);
        assert_eq!(cached_non_git.reserve(|| Ok(0)).unwrap(), 2);
        let fallback_sequence = fs::read(cached_non_git.sequence_path()).unwrap();
        let abandoned =
            fs::read(cached_non_git.reservations_dir().join(reservation_name(2))).unwrap();

        git(root, &["init", "-q"]);
        let nested = root.join("crates/core");
        fs::create_dir_all(&nested).unwrap();
        let error =
            IssueNumberSequence::new(&nested, &nested, &nested.join(STATE_DIR).join("issues"))
                .err()
                .expect("Git authority ignored an existing non-Git journal");
        assert!(error.to_string().contains("pre-Git issue-number authority"));
        assert!(!root.join(".git/usagi").exists());
        assert_eq!(
            fs::read(cached_non_git.sequence_path()).unwrap(),
            fallback_sequence
        );
        assert_eq!(
            fs::read(cached_non_git.reservations_dir().join(reservation_name(2))).unwrap(),
            abandoned
        );

        // An object constructed before `git init` can still use the old lock.
        // The new resolver must therefore keep refusing the split authority,
        // rather than allocating concurrently or reusing its abandoned gap.
        assert_eq!(cached_non_git.reserve(|| Ok(0)).unwrap(), 3);
        assert!(
            IssueNumberSequence::new(&nested, &nested, &nested.join(STATE_DIR).join("issues"))
                .is_err()
        );
        assert!(!root.join(".git/usagi").exists());
    }

    #[test]
    fn git_transition_rejects_a_raw_nested_non_git_authority_with_bytes_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let nested = root.join("crates/core");
        fs::create_dir_all(&nested).unwrap();
        let store = nested.join(STATE_DIR).join("issues");
        let cached_non_git = IssueNumberSequence::new(&nested, &nested, &store).unwrap();
        assert_eq!(cached_non_git.reserve(|| Ok(0)).unwrap(), 1);

        let sequence_before = fs::read(cached_non_git.sequence_path()).unwrap();
        let reservation = cached_non_git.reservations_dir().join(reservation_name(1));
        let reservation_before = fs::read(&reservation).unwrap();
        let legacy = legacy_sequence_for_store(&store);
        let legacy_before = fs::read(&legacy).unwrap();

        git(root, &["init", "-q"]);
        let error = IssueNumberSequence::new(&nested, root, &store)
            .err()
            .expect("Git authority ignored a raw nested pre-Git journal");
        assert!(error.to_string().contains("pre-Git issue-number authority"));
        assert_eq!(
            fs::read(cached_non_git.sequence_path()).unwrap(),
            sequence_before
        );
        assert_eq!(fs::read(&reservation).unwrap(), reservation_before);
        assert_eq!(fs::read(&legacy).unwrap(), legacy_before);
        assert!(!root.join(".git/usagi").exists());
    }

    #[test]
    fn linked_nested_caller_rejects_the_main_registered_fallback_authority() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "user.name", "Test"]);
        fs::write(root.join("README.md"), "root\n").unwrap();
        git(&root, &["add", "README.md"]);
        git(&root, &["commit", "-q", "-m", "init"]);

        let linked = tmp.path().join("external-linked");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "test-fallback-linked",
                linked.to_str().unwrap(),
            ],
        );
        let fallback = root.join(STATE_DIR).join(AUTHORITY_DIR);
        fs::create_dir_all(fallback.join(RESERVATIONS_DIR)).unwrap();
        fs::write(
            fallback.join(RESERVATIONS_DIR).join(reservation_name(515)),
            "515\n",
        )
        .unwrap();
        let nested = linked.join("crates/core");
        fs::create_dir_all(&nested).unwrap();

        let error =
            IssueNumberSequence::new(&nested, &nested, &nested.join(STATE_DIR).join("issues"))
                .err()
                .expect("linked caller ignored its fallback authority");
        assert!(error.to_string().contains("pre-Git issue-number authority"));
        assert!(!root.join(".git/usagi").exists());
        assert_eq!(
            fs::read_to_string(fallback.join(RESERVATIONS_DIR).join(reservation_name(515)))
                .unwrap(),
            "515\n"
        );
    }

    #[test]
    fn inherited_repo_scoping_environment_cannot_redirect_the_authority() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let foreign = tmp.path().join("foreign");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&foreign).unwrap();
        git(&root, &["init", "-q"]);
        git(&foreign, &["init", "-q"]);

        let foreign_git = foreign.join(".git");
        let result = tmp.path().join("resolver-result");
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["resolver_environment_process_helper", "--nocapture"])
            .env(RESOLVER_ROOT_ENV, &root)
            .env(RESOLVER_RESULT_ENV, &result)
            .env("GIT_DIR", &foreign_git)
            .env("GIT_WORK_TREE", &foreign)
            .env("GIT_INDEX_FILE", foreign_git.join("index"))
            .env("GIT_OBJECT_DIRECTORY", foreign_git.join("objects"))
            .env("GIT_COMMON_DIR", &foreign_git)
            .env("GIT_PREFIX", "wrong-prefix")
            .env("GIT_NAMESPACE", "wrong-namespace")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "resolver child failed: {stderr}");
        assert_eq!(
            fs::read_to_string(result).unwrap(),
            fs::canonicalize(root.join(".git"))
                .unwrap()
                .join("usagi/issue-numbers")
                .to_string_lossy()
        );
        assert!(!foreign_git.join("usagi").exists());
    }

    #[test]
    fn real_separate_git_dir_without_commondir_uses_the_validated_git_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let worktree = tmp.path().join("worktree");
        let git_dir = tmp.path().join("separate.git");
        let output = Command::new("git")
            .args(["init", "-q", "--separate-git-dir"])
            .arg(&git_dir)
            .arg(&worktree)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "git init failed: {stderr}");
        assert!(!git_dir.join("commondir").exists());
        let nested = worktree.join("crates/core");
        fs::create_dir_all(&nested).unwrap();
        let authority =
            IssueNumberSequence::new(&nested, &worktree, &nested.join(".usagi/issues")).unwrap();
        seed_sequence(&authority, 515);
        let common_legacy = git_dir.join("usagi-issue-sequence/next");
        fs::create_dir_all(common_legacy.parent().unwrap()).unwrap();
        fs::write(&common_legacy, legacy_sentinel(515)).unwrap();
        fs::write(authority.legacy_v2_migration_path(), "515\n").unwrap();

        assert_eq!(
            authority.worktree_root(),
            fs::canonicalize(&worktree).unwrap()
        );
        assert_eq!(
            authority.dir(),
            fs::canonicalize(&git_dir)
                .unwrap()
                .join("usagi/issue-numbers")
        );
        assert_eq!(authority.reserve(|| Ok(515)).unwrap(), 516);
        assert!(!nested.join(".usagi/issue-numbers").exists());
    }

    #[test]
    fn registered_worktree_replaced_by_a_foreign_repo_fails_without_touching_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "user.name", "Test"]);
        fs::write(root.join("README.md"), "root\n").unwrap();
        git(&root, &["add", "README.md"]);
        git(&root, &["commit", "-q", "-m", "init"]);

        let linked = tmp.path().join("linked");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "test-stale-linked-registration",
                linked.to_str().unwrap(),
            ],
        );
        fs::remove_dir_all(&linked).unwrap();
        fs::create_dir(&linked).unwrap();
        git(&linked, &["init", "-q"]);
        let foreign_legacy = legacy_sequence_for_store(&linked.join("nested/.usagi/issues"));
        fs::create_dir_all(foreign_legacy.parent().unwrap()).unwrap();
        fs::write(&foreign_legacy, "900\n").unwrap();
        let foreign_before = fs::read(&foreign_legacy).unwrap();

        let error = IssueNumberSequence::new(&root, &root, &root.join(STATE_DIR).join("issues"))
            .err()
            .expect("foreign replacement was accepted");
        assert!(error.to_string().contains("different Git common directory"));
        assert_eq!(fs::read(&foreign_legacy).unwrap(), foreign_before);
        assert!(!root.join(".git/usagi/issue-numbers").exists());
    }

    #[test]
    fn real_nested_main_and_linked_worktrees_fold_observed_store_local_legacy_state() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "user.name", "Test"]);
        fs::write(root.join("README.md"), "workspace\n").unwrap();
        git(&root, &["add", "README.md"]);
        git(&root, &["commit", "-q", "-m", "init"]);

        let root_authority = sequence(&root);
        assert_eq!(root_authority.reserve(|| Ok(0)).unwrap(), 1);
        let common = root.join(".git/usagi-issue-sequence/next");

        let nested_main = root.join("crates/core");
        let main_store = nested_main.join(".usagi/issues");
        let main_legacy = legacy_sequence_for_store(&main_store);
        fs::create_dir_all(main_legacy.parent().unwrap()).unwrap();
        fs::write(&main_legacy, "515\n").unwrap();
        let main = IssueNumberSequence::new(&nested_main, &root, &main_store).unwrap();
        assert_eq!(main.reserve(|| Ok(0)).unwrap(), 516);
        assert_eq!(fs::read_to_string(&common).unwrap(), legacy_sentinel(516));
        assert_eq!(
            fs::read_to_string(&main_legacy).unwrap(),
            legacy_sentinel(516)
        );
        assert!(!nested_main.join(".usagi/issue-numbers").exists());

        let sessions = root.join(".usagi/sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(root.join(".usagi/.gitignore"), "/*\n").unwrap();
        let linked = sessions.join("linked");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "test-linked-allocator",
                linked.to_str().unwrap(),
            ],
        );
        let nested_linked = linked.join("crates/core");
        let linked_store = nested_linked.join(".usagi/issues");
        let linked_legacy = legacy_sequence_for_store(&linked_store);
        fs::create_dir_all(linked_legacy.parent().unwrap()).unwrap();
        fs::write(&linked_legacy, "700\n").unwrap();
        let linked_authority =
            IssueNumberSequence::new(&nested_linked, &root, &linked_store).unwrap();

        assert_eq!(linked_authority.reserve(|| Ok(0)).unwrap(), 701);
        assert_eq!(fs::read_to_string(&common).unwrap(), legacy_sentinel(701));
        assert_eq!(
            fs::read_to_string(&linked_legacy).unwrap(),
            legacy_sentinel(701)
        );
        assert_eq!(
            fs::read_to_string(&main_legacy).unwrap(),
            legacy_sentinel(701)
        );
        assert_eq!(
            fs::canonicalize(linked_authority.dir()).unwrap(),
            fs::canonicalize(root.join(".git/usagi/issue-numbers")).unwrap()
        );
        assert!(!nested_linked.join(".usagi/issue-numbers").exists());

        // A materialized nested legacy path in a different registered
        // worktree is discovered even when it is outside workspace_root.
        let external = tmp.path().join("external-linked");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "test-external-linked-allocator",
                external.to_str().unwrap(),
            ],
        );
        let external_legacy =
            legacy_sequence_for_store(&external.join("tools/nested/.usagi/issues"));
        fs::create_dir_all(external_legacy.parent().unwrap()).unwrap();
        fs::write(&external_legacy, "800\n").unwrap();
        let main_again = IssueNumberSequence::new(&nested_main, &root, &main_store).unwrap();

        let expected = normalize_path_identity(external_legacy.parent().unwrap())
            .unwrap()
            .join(external_legacy.file_name().unwrap());
        assert!(
            main_again
                .legacy_v2_sequences()
                .unwrap()
                .iter()
                .any(|candidate| {
                    normalize_path_identity(candidate.parent().unwrap())
                        .is_ok_and(|parent| parent.join(candidate.file_name().unwrap()) == expected)
                })
        );
        assert_eq!(main_again.reserve(|| Ok(0)).unwrap(), 801);
        assert_eq!(
            fs::read_to_string(&external_legacy).unwrap(),
            legacy_sentinel(801)
        );
    }

    #[test]
    fn fixed_caller_discovers_and_fences_a_different_nested_old_v2_authority() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git(root, &["init", "-q"]);
        let common_authority = sequence(root);
        assert_eq!(common_authority.reserve(|| Ok(499)).unwrap(), 500);

        let caller = root.join("crates/core");
        let caller_store = caller.join(".usagi/issues");
        let caller_legacy = legacy_sequence_for_store(&caller_store);
        fs::create_dir_all(caller_legacy.parent().unwrap()).unwrap();
        fs::write(&caller_legacy, legacy_sentinel(500)).unwrap();

        let tracked = legacy_sequence_for_store(&root.join("tracked/.usagi/issues"));
        let untracked = legacy_sequence_for_store(&root.join("untracked/.usagi/issues"));
        let ignored = legacy_sequence_for_store(&root.join("ignored/.usagi/issues"));
        for path in [&tracked, &untracked, &ignored] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        fs::write(&tracked, legacy_sentinel(500)).unwrap();
        fs::write(&untracked, legacy_sentinel(500)).unwrap();
        fs::write(&ignored, "515\n").unwrap();
        git(
            root,
            &[
                "add",
                "-f",
                "tracked/.usagi/issues/usagi-issue-sequence/next",
            ],
        );
        fs::write(root.join(".git/info/exclude"), "ignored/.usagi/\n").unwrap();

        let authority = IssueNumberSequence::new(&caller, root, &caller_store).unwrap();
        let discovered = authority.legacy_v2_sequences().unwrap();
        for path in [&tracked, &untracked, &ignored] {
            let expected = normalize_path_identity(path.parent().unwrap())
                .unwrap()
                .join(path.file_name().unwrap());
            let path_display = path.display().to_string();
            assert!(
                discovered.iter().any(|candidate| {
                    normalize_path_identity(candidate.parent().unwrap())
                        .is_ok_and(|parent| parent.join(candidate.file_name().unwrap()) == expected)
                }),
                "missing {path_display} from {discovered:?}"
            );
        }
        assert_eq!(authority.reserve(|| Ok(0)).unwrap(), 516);
        for path in [&tracked, &untracked, &ignored] {
            assert_eq!(fs::read_to_string(path).unwrap(), legacy_sentinel(516));
        }
        assert_eq!(
            fs::read_to_string(&caller_legacy).unwrap(),
            legacy_sentinel(516)
        );
    }

    #[test]
    fn nested_source_derives_and_fences_a_still_missing_old_v2_authority() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git(root, &["init", "-q"]);
        let authority = sequence(root);
        assert_eq!(authority.reserve(|| Ok(799)).unwrap(), 800);

        let nested = root.join("tools/nested");
        let source = nested.join(".usagi/issues/800-known.md");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, "known nested source\n").unwrap();
        let local = legacy_sequence_for_store(&nested.join(".usagi/issues"));
        assert!(!local.exists());

        assert_eq!(authority.reserve(|| Ok(800)).unwrap(), 801);
        assert_eq!(fs::read_to_string(&local).unwrap(), legacy_sentinel(801));

        let before = fs::read(&local).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["old_v2_sequence_emulator_process_helper", "--nocapture"])
            .env(OLD_SEQUENCE_ENV, &local)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(fs::read(&local).unwrap(), before);
    }

    #[test]
    fn external_caller_fences_empty_direct_session_before_old_v2_can_allocate() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "user.name", "Test"]);
        fs::write(root.join("README.md"), b"workspace\n").unwrap();
        git(&root, &["add", "README.md"]);
        git(&root, &["commit", "-q", "-m", "init"]);

        let external = tmp.path().join("external-linked");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "test-external-empty-session",
                external.to_str().unwrap(),
            ],
        );
        let authority = sequence(&root);
        assert_eq!(authority.reserve(|| Ok(514)).unwrap(), 515);

        let direct_session = root.join(".usagi/sessions/plain");
        let direct_store = direct_session.join(STATE_DIR).join("issues");
        fs::create_dir_all(&direct_store).unwrap();
        let local = legacy_sequence_for_store(&direct_store);
        assert!(!local.exists());
        let external_authority = IssueNumberSequence::new(
            &external,
            &external,
            &external.join(STATE_DIR).join("issues"),
        )
        .unwrap();

        assert_eq!(external_authority.reserve(|| Ok(0)).unwrap(), 516);
        assert_eq!(fs::read_to_string(&local).unwrap(), legacy_sentinel(516));
        let reservation = external_authority
            .reservations_dir()
            .join(reservation_name(516));
        let preserved = [
            fs::read(&local).unwrap(),
            fs::read(external_authority.sequence_path()).unwrap(),
            fs::read(external_authority.legacy_v2_migration_path()).unwrap(),
            fs::read(&reservation).unwrap(),
        ];
        let result = tmp.path().join("old-v2-result.json");

        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "old_v2_compatibility_emulator_process_helper",
                "--nocapture",
            ])
            .current_dir(&direct_session)
            .env(OLD_V2_EMULATOR_RESULT_ENV, &result)
            .output()
            .unwrap();

        assert!(!output.status.success());
        assert!(!result.exists());
        assert_eq!(fs::read(&local).unwrap(), preserved[0]);
        assert_eq!(
            fs::read(external_authority.sequence_path()).unwrap(),
            preserved[1]
        );
        assert_eq!(
            fs::read(external_authority.legacy_v2_migration_path()).unwrap(),
            preserved[2]
        );
        assert_eq!(fs::read(&reservation).unwrap(), preserved[3]);

        let rogue = root.join(".usagi/sessions/rogue");
        fs::create_dir_all(rogue.join(".git")).unwrap();
        let error = external_authority.reserve(|| Ok(0)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unregistered direct session has an independent Git authority")
        );
        assert_eq!(fs::read(&local).unwrap(), preserved[0]);
        assert_eq!(
            fs::read(external_authority.sequence_path()).unwrap(),
            preserved[1]
        );
        assert_eq!(
            fs::read(external_authority.legacy_v2_migration_path()).unwrap(),
            preserved[2]
        );
        assert_eq!(fs::read(&reservation).unwrap(), preserved[3]);
        assert!(
            !external_authority
                .reservations_dir()
                .join(reservation_name(517))
                .exists()
        );
    }

    #[test]
    fn malformed_git_indirection_and_common_dir_fail_before_creating_authority() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        assert!(IssueNumberSequence::new(tmp.path(), tmp.path(), tmp.path()).is_err());
        assert!(!tmp.path().join(".git/usagi").exists());
        assert!(!tmp.path().join(".usagi").exists());

        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".git"), "not a gitdir\n").unwrap();
        assert!(IssueNumberSequence::new(tmp.path(), tmp.path(), tmp.path()).is_err());
        assert!(!tmp.path().join(".usagi").exists());

        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".git"), "gitdir: missing-private\n").unwrap();
        assert!(IssueNumberSequence::new(tmp.path(), tmp.path(), tmp.path()).is_err());
        assert!(!tmp.path().join("missing-private").exists());
        assert!(!tmp.path().join(".usagi").exists());

        let tmp = tempfile::tempdir().unwrap();
        let private = tmp.path().join("private");
        fs::create_dir(&private).unwrap();
        fs::write(tmp.path().join(".git"), "gitdir: private\n").unwrap();
        fs::write(private.join("commondir"), "../missing-common\n").unwrap();
        assert!(IssueNumberSequence::new(tmp.path(), tmp.path(), tmp.path()).is_err());
        assert!(!tmp.path().join("missing-common").exists());
        assert!(!private.join("usagi").exists());
        assert!(!tmp.path().join(".usagi").exists());

        fs::write(private.join("commondir"), "\n").unwrap();
        assert!(IssueNumberSequence::new(tmp.path(), tmp.path(), tmp.path()).is_err());
        fs::remove_file(private.join("commondir")).unwrap();
        fs::create_dir(private.join("commondir")).unwrap();
        assert!(IssueNumberSequence::new(tmp.path(), tmp.path(), tmp.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_common_dir_indirection_fails_without_creating_an_authority() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let worktree = tmp.path().join("worktree");
        let git_dir = tmp.path().join("separate.git");
        let output = Command::new("git")
            .args(["init", "-q", "--separate-git-dir"])
            .arg(&git_dir)
            .arg(&worktree)
            .output()
            .unwrap();
        assert!(output.status.success());
        symlink("missing-common", git_dir.join("commondir")).unwrap();

        assert!(IssueNumberSequence::new(&worktree, &worktree, &worktree).is_err());
        assert!(!git_dir.join("usagi").exists());
        assert!(!worktree.join(".usagi").exists());
    }

    #[test]
    fn old_v2_sequence_emulator_process_helper() {
        let Some(sequence) = std::env::var_os(OLD_SEQUENCE_ENV).map(PathBuf::from) else {
            return;
        };
        let _lock = StoreLock::acquire(sequence.parent().unwrap()).unwrap();
        let current = fs::read_to_string(&sequence)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        write_text_atomic(&sequence, &format!("{}\n", current.checked_add(1).unwrap())).unwrap();

        wait_for_emulator_release(
            OLD_READY_ENV,
            OLD_RELEASE_ENV,
            "parent never released old allocator",
        );
    }

    /// Compatibility emulator for the immediately preceding v2 allocator.
    ///
    /// This intentionally runs in a real subprocess and derives its legacy
    /// sequence from that process's raw cwd. It is not an historical binary;
    /// the resolver, filename-only source maximum, lock, parse, increment, and
    /// atomic write below mirror the old `IssueStore` implementation.
    #[test]
    fn old_v2_compatibility_emulator_process_helper() {
        let Some(result_path) = std::env::var_os(OLD_V2_EMULATOR_RESULT_ENV).map(PathBuf::from)
        else {
            return;
        };
        let raw_cwd = std::env::current_dir().unwrap();
        let allocation_dir = old_v2_compatibility_allocation_dir(&raw_cwd).unwrap();
        let sequence = allocation_dir.join(LEGACY_V2_FILE);
        if let Some(resolved_path) =
            std::env::var_os(OLD_V2_EMULATOR_RESOLVED_ENV).map(PathBuf::from)
        {
            fs::write(&resolved_path, serde_json::to_vec(&sequence).unwrap()).unwrap();
        }

        let _lock = StoreLock::acquire(&allocation_dir).unwrap();
        let reserved = old_v2_compatibility_reserved(&sequence).unwrap();
        let number = reserved
            .max(old_v2_compatibility_max_number(&raw_cwd).unwrap())
            .checked_add(1)
            .unwrap();
        write_text_atomic(&sequence, &format!("{number}\n")).unwrap();
        fs::write(
            &result_path,
            serde_json::to_vec(&OldV2EmulatorResult { sequence, number }).unwrap(),
        )
        .unwrap();

        wait_for_emulator_release(
            OLD_V2_EMULATOR_READY_ENV,
            OLD_V2_EMULATOR_RELEASE_ENV,
            "parent never released old-v2 compatibility emulator",
        );
    }

    #[test]
    fn old_v1_emulator_process_helper() {
        let Some(root) = std::env::var_os(OLD_V1_ROOT_ENV).map(PathBuf::from) else {
            return;
        };
        let result = PathBuf::from(std::env::var_os(OLD_V1_RESULT_ENV).unwrap());
        let number = v1_reserve(&sequence(&root)).unwrap();
        fs::write(result, format!("{number}\n")).unwrap();
    }

    #[test]
    fn resolver_environment_process_helper() {
        let Some(root) = std::env::var_os(RESOLVER_ROOT_ENV).map(PathBuf::from) else {
            return;
        };
        let result = PathBuf::from(std::env::var_os(RESOLVER_RESULT_ENV).unwrap());
        let authority =
            IssueNumberSequence::new(&root, &root, &root.join(".usagi/issues")).unwrap();
        fs::write(result, authority.dir().to_string_lossy().as_bytes()).unwrap();
    }

    #[test]
    fn migrated_authority_is_usable_by_an_old_v1_emulator_process_then_fixed_v2() {
        let tmp = tempfile::tempdir().unwrap();
        let authority = git_sequence(tmp.path());
        let legacy = only_legacy(&authority);
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, "515\n").unwrap();

        assert_eq!(authority.reserve(|| Ok(0)).unwrap(), 516);
        let fence = fs::read(&legacy).unwrap();
        let result = tmp.path().join("old-v1-result");
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["old_v1_emulator_process_helper", "--nocapture"])
            .env(OLD_V1_ROOT_ENV, tmp.path())
            .env(OLD_V1_RESULT_ENV, &result)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "old v1 failed: {stderr}");
        assert_eq!(fs::read_to_string(result).unwrap(), "517\n");
        assert_eq!(fs::read(&legacy).unwrap(), fence);
        assert_eq!(authority.reserve(|| Ok(0)).unwrap(), 518);
    }

    #[cfg(unix)]
    #[test]
    fn dangling_sessions_tree_fails_before_sequence_or_legacy_mutation() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join(STATE_DIR)).unwrap();
        let sessions = root.join(STATE_DIR).join(SESSIONS_DIR);
        symlink("missing-sessions", &sessions).unwrap();
        let authority = sequence(root);

        assert!(authority.reserve(|| Ok(900)).is_err());
        assert_eq!(
            fs::read_link(&sessions).unwrap(),
            PathBuf::from("missing-sessions")
        );
        assert!(!authority.sequence_path().exists());
        assert!(!authority.reservations_dir().exists());
        assert!(!legacy_sequence_for_store(&root.join(".usagi/issues")).exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_session_entry_fails_before_legacy_or_authority_mutation() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let real_session = root.join("real-session");
        let source = real_session.join(".usagi/issues/800-real.md");
        let legacy = legacy_sequence_for_store(&real_session.join(".usagi/issues"));
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&source, b"real session source\n").unwrap();
        fs::write(&legacy, b"800\n").unwrap();
        let sessions = root.join(STATE_DIR).join(SESSIONS_DIR);
        fs::create_dir_all(&sessions).unwrap();
        let session_entry = sessions.join("linked");
        symlink(&real_session, &session_entry).unwrap();

        let authority = sequence(root);
        seed_sequence(&authority, 500);
        seed_reservation(&authority, 500);
        let sequence_before = fs::read(authority.sequence_path()).unwrap();
        let reservation = authority.reservations_dir().join(reservation_name(500));
        let reservation_before = fs::read(&reservation).unwrap();
        let source_before = fs::read(&source).unwrap();
        let legacy_before = fs::read(&legacy).unwrap();

        let error = authority.reserve(|| Ok(900)).unwrap_err();
        assert!(error.to_string().contains("session entry is a symlink"));
        assert_eq!(
            fs::read(authority.sequence_path()).unwrap(),
            sequence_before
        );
        assert_eq!(fs::read(&reservation).unwrap(), reservation_before);
        assert_eq!(fs::read(&source).unwrap(), source_before);
        assert_eq!(fs::read(&legacy).unwrap(), legacy_before);
        assert!(
            fs::symlink_metadata(&session_entry)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!authority.legacy_v2_migration_path().exists());
        assert!(
            !authority
                .reservations_dir()
                .join(reservation_name(501))
                .exists()
        );
        assert!(!legacy_sequence_for_store(&root.join(".usagi/issues")).exists());
    }

    #[test]
    fn nested_old_v2_emulator_reservation_is_folded_by_the_production_store() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git(root, &["init", "-q"]);
        let nested = root.join("tools/nested");
        fs::create_dir_all(&nested).unwrap();

        let authority = sequence(root);
        seed_sequence(&authority, 515);
        let common = authority.shared_legacy_sequence().unwrap().to_path_buf();
        fs::create_dir_all(common.parent().unwrap()).unwrap();
        fs::write(&common, legacy_sentinel(515)).unwrap();
        fs::write(authority.legacy_v2_migration_path(), b"515\n").unwrap();
        let local_store = nested.join(STATE_DIR).join("issues");
        let local = legacy_sequence_for_store(&local_store);
        fs::create_dir_all(&local_store).unwrap();
        let source = local_store.join("515-old-v2-source-max.md");
        fs::write(
            &source,
            b"---\nnumber: 515\ntitle: Old v2 source maximum\nstatus: todo\npriority: medium\nlabels: []\ndependson: []\nrelated: []\ncreated_at: 2026-07-22T00:00:00+00:00\nupdated_at: 2026-07-22T00:00:00+00:00\n---\n\nCompatibility fixture.\n",
        )
        .unwrap();
        let source_before = fs::read(&source).unwrap();
        assert!(!local.exists());

        let result = root.join("old-v2-emulator-result");
        let resolved = root.join("old-v2-emulator-resolved");
        let ready = root.join("old-v2-emulator-ready");
        let release = root.join("old-v2-emulator-release");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "old_v2_compatibility_emulator_process_helper",
                "--nocapture",
            ])
            .current_dir(&nested)
            .env(OLD_V2_EMULATOR_RESULT_ENV, &result)
            .env(OLD_V2_EMULATOR_RESOLVED_ENV, &resolved)
            .env(OLD_V2_EMULATOR_READY_ENV, &ready)
            .env(OLD_V2_EMULATOR_RELEASE_ENV, &release)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        // Force at least one poll iteration so the wait body stays covered even
        // when the spawned child publishes its file before the first check.
        let mut first_poll = true;
        while first_poll || !ready.exists() {
            first_poll = false;
            assert!(
                Instant::now() < deadline,
                "old-v2 compatibility emulator did not acquire its local lock"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let observed: OldV2EmulatorResult =
            serde_json::from_slice(&fs::read(&result).unwrap()).unwrap();
        let resolved_path: PathBuf = serde_json::from_slice(&fs::read(&resolved).unwrap()).unwrap();
        assert_eq!(observed.number, 516);
        assert_eq!(observed.sequence, resolved_path);
        assert_eq!(
            fs::canonicalize(&observed.sequence).unwrap(),
            fs::canonicalize(&local).unwrap()
        );
        assert_eq!(fs::read_to_string(&local).unwrap(), "516\n");
        assert_eq!(fs::read(&source).unwrap(), source_before);

        let (reservation, receiver) = start_reservation_blocked_on_legacy(&nested, &authority);

        fs::write(&release, b"release\n").unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(
            receiver
                .recv_timeout(Duration::from_secs(15))
                .unwrap()
                .unwrap(),
            517
        );
        reservation.join().unwrap();
        assert_eq!(fs::read(&source).unwrap(), source_before);
        assert_eq!(fs::read_to_string(&local).unwrap(), legacy_sentinel(517));
        assert_eq!(fs::read_to_string(&common).unwrap(), legacy_sentinel(517));
        assert_eq!(
            fs::read_to_string(authority.legacy_v2_migration_path()).unwrap(),
            "517\n"
        );
        assert!(
            authority
                .reservations_dir()
                .join(reservation_name(517))
                .is_file()
        );
    }

    #[test]
    fn fixed_source_derived_fence_rejects_a_queued_nested_old_v2_emulator() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git(root, &["init", "-q"]);
        let nested = root.join("tools/nested");
        let local_store = nested.join(STATE_DIR).join("issues");
        fs::create_dir_all(&local_store).unwrap();
        let source = local_store.join("515-materialized.md");
        fs::write(
            &source,
            b"---\nnumber: 515\ntitle: Materialized nested source\nstatus: todo\npriority: medium\nlabels: []\ndependson: []\nrelated: []\ncreated_at: 2026-07-22T00:00:00+00:00\nupdated_at: 2026-07-22T00:00:00+00:00\n---\n\nCompatibility fixture.\n",
        )
        .unwrap();
        let source_before = fs::read(&source).unwrap();
        let local = legacy_sequence_for_store(&local_store);
        assert!(!local.exists());

        let authority = sequence(root);
        seed_sequence(&authority, 515);
        let common = authority.shared_legacy_sequence().unwrap().to_path_buf();
        fs::create_dir_all(common.parent().unwrap()).unwrap();
        fs::write(&common, legacy_sentinel(515)).unwrap();
        fs::write(authority.legacy_v2_migration_path(), b"515\n").unwrap();

        let root_for_thread = root.to_path_buf();
        let (blocked_sender, blocked_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let reservation = thread::spawn(move || {
            sequence(&root_for_thread).reserve_observing(
                || Ok(515),
                || {
                    blocked_sender.send(()).unwrap();
                    release_receiver.recv().unwrap();
                },
            )
        });
        blocked_receiver
            .recv_timeout(Duration::from_secs(15))
            .unwrap();
        assert_eq!(fs::read_to_string(&local).unwrap(), legacy_sentinel(515));
        assert_eq!(
            authority.read_sequence().unwrap(),
            SequenceState::MigrationBlocked(515)
        );

        let result = root.join("queued-old-v2-emulator-result");
        let resolved = root.join("queued-old-v2-emulator-resolved");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "old_v2_compatibility_emulator_process_helper",
                "--nocapture",
            ])
            .current_dir(&nested)
            .env(OLD_V2_EMULATOR_RESULT_ENV, &result)
            .env(OLD_V2_EMULATOR_RESOLVED_ENV, &resolved)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        // Force at least one completed poll before accepting the published file
        // so this wait path remains covered even when the child publishes early.
        let mut completed_one_poll = false;
        loop {
            assert!(
                Instant::now() < deadline,
                "queued old-v2 compatibility emulator did not resolve its local authority"
            );
            thread::sleep(Duration::from_millis(10));
            if completed_one_poll && resolved.exists() {
                break;
            }
            completed_one_poll = true;
        }
        assert!(child.try_wait().unwrap().is_none());
        assert!(!result.exists());

        release_sender.send(()).unwrap();
        assert_eq!(reservation.join().unwrap().unwrap(), 516);
        assert!(!child.wait().unwrap().success());
        assert!(!result.exists());
        let resolved_path: PathBuf = serde_json::from_slice(&fs::read(&resolved).unwrap()).unwrap();
        assert_eq!(
            fs::canonicalize(&resolved_path).unwrap(),
            fs::canonicalize(&local).unwrap()
        );
        assert_eq!(fs::read(&source).unwrap(), source_before);
        assert_eq!(fs::read_to_string(&local).unwrap(), legacy_sentinel(516));
        assert_eq!(fs::read_to_string(&common).unwrap(), legacy_sentinel(516));
        assert_eq!(
            fs::read_to_string(authority.legacy_v2_migration_path()).unwrap(),
            "516\n"
        );
        assert!(
            authority
                .reservations_dir()
                .join(reservation_name(516))
                .is_file()
        );
    }

    #[test]
    fn migration_waits_for_and_then_fences_an_old_v2_emulator_process() {
        let tmp = tempfile::tempdir().unwrap();
        let authority = git_sequence(tmp.path());
        let legacy = only_legacy(&authority);
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, "515\n").unwrap();
        let ready = tmp.path().join("old-ready");
        let release = tmp.path().join("old-release");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["old_v2_sequence_emulator_process_helper", "--nocapture"])
            .env(OLD_SEQUENCE_ENV, &legacy)
            .env(OLD_READY_ENV, &ready)
            .env(OLD_RELEASE_ENV, &release)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        // Force at least one poll iteration so the wait body stays covered even
        // when the spawned child publishes its file before the first check.
        let mut first_poll = true;
        while first_poll || !ready.exists() {
            first_poll = false;
            assert!(
                Instant::now() < deadline,
                "old allocator did not acquire its lock"
            );
            thread::sleep(Duration::from_millis(10));
        }

        let root = tmp.path().to_path_buf();
        let (sender, receiver) = mpsc::channel();
        let reservation = thread::spawn(move || {
            let authority = sequence(&root);
            sender.send(authority.reserve(|| Ok(0))).unwrap();
        });
        thread::sleep(Duration::from_millis(50));
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        fs::write(&release, "release\n").unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(
            receiver
                .recv_timeout(Duration::from_secs(15))
                .unwrap()
                .unwrap(),
            517
        );
        reservation.join().unwrap();
        assert_eq!(fs::read_to_string(&legacy).unwrap(), legacy_sentinel(517));

        let before = fs::read(&legacy).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["old_v2_sequence_emulator_process_helper", "--nocapture"])
            .env(OLD_SEQUENCE_ENV, &legacy)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(fs::read(&legacy).unwrap(), before);
    }

    #[test]
    fn queued_old_allocator_process_fails_after_new_allocator_publishes_its_fence() {
        let tmp = tempfile::tempdir().unwrap();
        let authority = git_sequence(tmp.path());
        let legacy = only_legacy(&authority);
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, "515\n").unwrap();
        let root = tmp.path().to_path_buf();
        let (blocked_sender, blocked_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let reservation = thread::spawn(move || {
            sequence(&root).reserve_observing(
                || Ok(0),
                || {
                    blocked_sender.send(()).unwrap();
                    release_receiver.recv().unwrap();
                },
            )
        });
        blocked_receiver
            .recv_timeout(Duration::from_secs(15))
            .unwrap();
        assert_eq!(
            authority.read_sequence().unwrap(),
            SequenceState::MigrationBlocked(515)
        );

        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["old_v2_sequence_emulator_process_helper", "--nocapture"])
            .env(OLD_SEQUENCE_ENV, &legacy)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        thread::sleep(Duration::from_millis(50));
        assert!(child.try_wait().unwrap().is_none());

        release_sender.send(()).unwrap();
        assert_eq!(reservation.join().unwrap().unwrap(), 516);
        assert!(!child.wait().unwrap().success());
        assert_eq!(fs::read_to_string(&legacy).unwrap(), legacy_sentinel(516));
        assert_eq!(
            authority.read_sequence().unwrap(),
            SequenceState::Normal(516)
        );
        assert_eq!(
            fs::read_to_string(authority.legacy_v2_migration_path()).unwrap(),
            "516\n"
        );
    }

    #[test]
    fn v1_leading_handshake_fences_a_queued_old_v2_emulator_process() {
        let tmp = tempfile::tempdir().unwrap();
        let authority = git_sequence(tmp.path());
        let legacy = only_legacy(&authority);
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, "500\n").unwrap();
        seed_sequence(&authority, 800);

        let root = tmp.path().to_path_buf();
        let (blocked_sender, blocked_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let reservation = thread::spawn(move || {
            sequence(&root).reserve_observing(
                || Ok(0),
                || {
                    blocked_sender.send(()).unwrap();
                    release_receiver.recv().unwrap();
                },
            )
        });
        blocked_receiver
            .recv_timeout(Duration::from_secs(15))
            .unwrap();
        assert_eq!(fs::read_to_string(&legacy).unwrap(), legacy_sentinel(800));
        assert_eq!(
            authority.read_sequence().unwrap(),
            SequenceState::MigrationBlocked(800)
        );

        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["old_v2_sequence_emulator_process_helper", "--nocapture"])
            .env(OLD_SEQUENCE_ENV, &legacy)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        thread::sleep(Duration::from_millis(50));
        assert!(child.try_wait().unwrap().is_none());

        release_sender.send(()).unwrap();
        assert_eq!(reservation.join().unwrap().unwrap(), 801);
        assert!(!child.wait().unwrap().success());
        assert_eq!(fs::read_to_string(&legacy).unwrap(), legacy_sentinel(801));
        assert_eq!(
            authority.read_sequence().unwrap(),
            SequenceState::Normal(801)
        );
    }

    #[test]
    fn nested_store_local_old_v2_emulator_is_folded_after_the_common_fence() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git(root, &["init", "-q"]);
        let common_authority = sequence(root);
        assert_eq!(common_authority.reserve(|| Ok(499)).unwrap(), 500);
        let common = only_legacy(&common_authority);

        let nested = root.join("crates/core");
        let local_store = nested.join(".usagi/issues");
        let local = legacy_sequence_for_store(&local_store);
        fs::create_dir_all(local.parent().unwrap()).unwrap();
        fs::write(&local, "515\n").unwrap();
        let ready = root.join("local-old-ready");
        let release = root.join("local-old-release");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["old_v2_sequence_emulator_process_helper", "--nocapture"])
            .env(OLD_SEQUENCE_ENV, &local)
            .env(OLD_READY_ENV, &ready)
            .env(OLD_RELEASE_ENV, &release)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        // Force at least one poll iteration so the wait body stays covered even
        // when the spawned child publishes its file before the first check.
        let mut first_poll = true;
        while first_poll || !ready.exists() {
            first_poll = false;
            assert!(
                Instant::now() < deadline,
                "local old allocator was not ready"
            );
            thread::sleep(Duration::from_millis(10));
        }

        let nested_for_thread = nested.clone();
        let root_for_thread = root.to_path_buf();
        let local_store_for_thread = local_store.clone();
        let reservation = thread::spawn(move || {
            IssueNumberSequence::new(
                &nested_for_thread,
                &root_for_thread,
                &local_store_for_thread,
            )
            .unwrap()
            .reserve(|| Ok(0))
        });
        thread::sleep(Duration::from_millis(50));
        assert!(!reservation.is_finished());
        fs::write(&release, "release\n").unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(reservation.join().unwrap().unwrap(), 517);
        assert_eq!(fs::read_to_string(&local).unwrap(), legacy_sentinel(517));
        assert_eq!(fs::read_to_string(&common).unwrap(), legacy_sentinel(517));
    }

    #[test]
    fn git_rejects_a_tab_gitdir_format_before_authority_creation() {
        let tmp = tempfile::tempdir().unwrap();
        let worktree = tmp.path().join("worktree");
        let private = tmp.path().join("private");
        let output = Command::new("git")
            .args(["init", "-q", "--separate-git-dir"])
            .arg(&private)
            .arg(&worktree)
            .output()
            .unwrap();
        assert!(output.status.success());
        fs::write(
            worktree.join(".git"),
            format!("gitdir:\t{}\n", private.display()),
        )
        .unwrap();

        assert!(IssueNumberSequence::new(&worktree, &worktree, &worktree).is_err());
        assert!(!private.join("usagi").exists());
        assert!(!worktree.join(".usagi").exists());
    }
}

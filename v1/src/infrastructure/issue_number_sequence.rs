//! Workspace-global, crash-durable issue number reservations.
//!
//! Issue markdown lives in each Git worktree, so a per-store lock cannot
//! serialize allocation across sibling worktrees. This authority lives below
//! Git's common directory (shared by every linked worktree) and combines a
//! high-water sequence with durable per-number reservation markers. The markers
//! let a missing, stale, or corrupt sequence recover without reusing a number
//! that was reserved immediately before a process crashed.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::infrastructure::git;
use crate::infrastructure::json_file;
use crate::infrastructure::repo_paths::STATE_DIR;
use crate::infrastructure::store_lock::StoreLock;

const AUTHORITY_DIR: &str = "issue-numbers";
const SEQUENCE_FILE: &str = "sequence.json";
const RESERVATIONS_DIR: &str = "reservations";
const RESERVATION_SUFFIX: &str = ".reserved";

#[derive(Deserialize, Serialize)]
struct Sequence {
    last_reserved: u32,
}

/// The process/worktree-shared authority that reserves issue numbers.
pub struct IssueNumberSequence {
    dir: PathBuf,
}

impl IssueNumberSequence {
    /// Resolve the authority shared by every worktree in the same Git repository.
    /// Non-Git test/workspace directories fall back to the workspace's `.usagi`.
    pub fn new(repo_root: &Path, workspace_root: &Path) -> Self {
        let dir = git::git_common_dir(repo_root)
            .map(|common| common.join("usagi").join(AUTHORITY_DIR))
            .unwrap_or_else(|| workspace_root.join(STATE_DIR).join(AUTHORITY_DIR));
        Self { dir }
    }

    /// Directory containing the authority lock, sequence, and reservation journal.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn sequence_path(&self) -> PathBuf {
        self.dir.join(SEQUENCE_FILE)
    }

    fn reservations_dir(&self) -> PathBuf {
        self.dir.join(RESERVATIONS_DIR)
    }

    /// Reserve one number while holding the common authority lock.
    ///
    /// `max_existing` runs after the lock is acquired so every worktree is
    /// scanned in the same critical section as sequence migration. The durable
    /// marker is written before the sequence: if the process dies at any later
    /// point, the reservation remains visible and is never handed out again.
    pub fn reserve<F>(&self, max_existing: F) -> Result<u32>
    where
        F: FnOnce() -> Result<u32>,
    {
        let _lock = StoreLock::acquire(&self.dir)?;
        let existing = max_existing()?;
        let sequence = self.read_sequence()?.unwrap_or(0);
        let journal = self.max_reservation()?;
        let number = existing
            .max(sequence)
            .max(journal)
            .checked_add(1)
            .context("issue number space is exhausted")?;

        let reservations = self.reservations_dir();
        fs::create_dir_all(&reservations)
            .context(format!("failed to create {}", reservations.display()))?;
        let marker = reservations.join(format!("{number:010}{RESERVATION_SUFFIX}"));
        json_file::write_text_atomic(&marker, &format!("{number}\n"))?;
        json_file::write_versioned(
            &self.dir,
            &self.sequence_path(),
            &Sequence {
                last_reserved: number,
            },
        )?;
        Ok(number)
    }

    /// Missing and syntactically corrupt sequences are migrated from the
    /// existing issue/marker maxima. Other IO failures remain hard errors.
    fn read_sequence(&self) -> Result<Option<u32>> {
        let path = self.sequence_path();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context(format!("failed to read {}", path.display())),
        };
        Ok(serde_json::from_str::<Sequence>(&text)
            .ok()
            .map(|sequence| sequence.last_reserved))
    }

    fn max_reservation(&self) -> Result<u32> {
        let entries = match fs::read_dir(self.reservations_dir()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error).context("failed to read issue reservations"),
        };
        let mut max = 0;
        for entry in entries {
            let path = entry
                .context("failed to read an issue reservation entry")?
                .path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(number) = name
                .strip_suffix(RESERVATION_SUFFIX)
                .and_then(|number| number.parse::<u32>().ok())
            else {
                continue;
            };
            max = max.max(number);
        }
        Ok(max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sequence(root: &Path) -> IssueNumberSequence {
        IssueNumberSequence::new(root, root)
    }

    #[test]
    fn missing_sequence_migrates_from_existing_maximum() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(sequence(tmp.path()).reserve(|| Ok(41)).unwrap(), 42);
    }

    #[test]
    fn stale_sequence_migrates_from_existing_maximum() {
        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        assert_eq!(authority.reserve(|| Ok(0)).unwrap(), 1);
        assert_eq!(authority.reserve(|| Ok(9)).unwrap(), 10);
    }

    #[test]
    fn corrupt_sequence_migrates_from_existing_and_reservation_maxima() {
        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        assert_eq!(authority.reserve(|| Ok(4)).unwrap(), 5);
        fs::write(authority.sequence_path(), "not json").unwrap();
        assert_eq!(authority.reserve(|| Ok(8)).unwrap(), 9);
    }

    #[test]
    fn an_uncommitted_reservation_is_never_reused_after_a_crash() {
        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        let abandoned = authority.reserve(|| Ok(0)).unwrap();
        assert_eq!(abandoned, 1);

        // No issue file was written, and even losing the sequence does not make
        // the abandoned number reusable because its durable marker is the journal.
        fs::remove_file(authority.sequence_path()).unwrap();
        assert_eq!(authority.reserve(|| Ok(0)).unwrap(), 2);
    }

    #[test]
    fn a_high_sequence_is_not_reused_when_files_and_markers_are_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        fs::create_dir_all(authority.dir()).unwrap();
        json_file::write_versioned(
            authority.dir(),
            &authority.sequence_path(),
            &Sequence { last_reserved: 50 },
        )
        .unwrap();
        assert_eq!(authority.reserve(|| Ok(3)).unwrap(), 51);
    }

    #[test]
    fn malformed_reservation_names_are_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        let reservations = authority.reservations_dir();
        fs::create_dir_all(&reservations).unwrap();
        fs::write(reservations.join("README"), "x").unwrap();
        fs::write(reservations.join("not-a-number.reserved"), "x").unwrap();
        fs::write(reservations.join("0000000012.reserved"), "12\n").unwrap();
        assert_eq!(authority.reserve(|| Ok(2)).unwrap(), 13);
    }

    #[test]
    fn unreadable_sequence_and_reservation_paths_are_hard_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        fs::create_dir_all(authority.sequence_path()).unwrap();
        assert!(authority
            .reserve(|| Ok(0))
            .unwrap_err()
            .to_string()
            .contains("failed to read"));

        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        fs::create_dir_all(authority.dir()).unwrap();
        fs::write(authority.reservations_dir(), "not a directory").unwrap();
        assert!(authority
            .reserve(|| Ok(0))
            .unwrap_err()
            .to_string()
            .contains("failed to read issue reservations"));
    }

    #[test]
    fn exhausted_number_space_is_reported_without_wrapping() {
        let tmp = tempfile::tempdir().unwrap();
        let authority = sequence(tmp.path());
        let error = authority.reserve(|| Ok(u32::MAX)).unwrap_err();
        assert!(error
            .to_string()
            .contains("issue number space is exhausted"));
    }
}

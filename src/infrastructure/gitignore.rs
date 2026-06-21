//! Low-level `.gitignore` editing for usagi's per-project metadata directory.
//!
//! The usecase layer ([`crate::usecase::project`]) only expresses the intent —
//! "keep `.usagi/` out of git while the shared `issues/` and `memory/`
//! directories stay tracked". The actual byte-level work (the rule text, line
//! filtering, trailing-blank trimming, and write-back) lives here.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::infrastructure::repo_paths::STATE_DIR;

/// The self-contained `.gitignore` usagi writes inside each repository's
/// `.usagi/` directory. Patterns are relative to `.usagi/`: ignore everything,
/// but keep this `.gitignore` and the shared `issues/` and `memory/` directories
/// tracked, while still excluding their rebuildable `index.json` caches and the
/// per-store `.lock` files used for cross-process write locking.
///
/// Task issues and agent memories are meant to be committed and shared with the
/// team; the machine-local state (`state.json`, `settings.json`, `history.json`,
/// `sessions/`), the derived indexes, and the lock files stay ignored. Keeping
/// the rules inside `.usagi/` leaves the repository-root `.gitignore` untouched.
///
/// The `index.json` / `.lock` filenames are spelled out here for readability;
/// `gitignore_covers_the_derived_and_lock_files` asserts they stay in step with
/// the constants that actually name those files
/// ([`issue_store::INDEX_FILE`](crate::infrastructure::issue_store) /
/// [`memory_store::INDEX_FILE`](crate::infrastructure::memory_store) /
/// [`store_lock::LOCK_FILE_NAME`](crate::infrastructure::store_lock)), so renaming
/// one without the other fails the test rather than leaking a file into git.
pub const USAGI_GITIGNORE: &str = "/*\n!/.gitignore\n!/issues/\n/issues/index.json\n/issues/.lock\n!/memory/\n/memory/index.json\n/memory/.lock\n";

/// Write [`USAGI_GITIGNORE`] to `<repo>/.usagi/.gitignore`, creating the
/// directory when absent. Idempotent: if the file already holds the current
/// content it is left untouched.
pub fn write_usagi_gitignore(repo: &Path) -> Result<()> {
    let dir = repo.join(STATE_DIR);
    fs::create_dir_all(&dir).context(format!("failed to create {}", dir.display()))?;

    let gitignore = dir.join(".gitignore");
    if fs::read_to_string(&gitignore).is_ok_and(|c| c == USAGI_GITIGNORE) {
        return Ok(());
    }
    fs::write(&gitignore, USAGI_GITIGNORE)
        .context(format!("failed to write {}", gitignore.display()))
}

/// Remove usagi-managed lines from the repository-root `.gitignore`, preserving
/// all other content. Does nothing when the file is absent or carries no such
/// lines, so it never creates a root `.gitignore` of its own.
pub fn strip_legacy_root_entries(repo: &Path) -> Result<()> {
    let gitignore = repo.join(".gitignore");

    let existing = match fs::read_to_string(&gitignore) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).context(format!("failed to read {}", gitignore.display())),
    };
    if !existing.lines().any(is_legacy_root_ignore_line) {
        return Ok(());
    }

    let mut kept: Vec<&str> = existing
        .lines()
        .filter(|l| !is_legacy_root_ignore_line(l))
        .collect();
    // Trim trailing blank lines left behind so the file ends cleanly.
    while kept.last().is_some_and(|l| l.trim().is_empty()) {
        kept.pop();
    }

    let mut out = String::new();
    for line in kept {
        out.push_str(line);
        out.push('\n');
    }
    fs::write(&gitignore, out).context(format!("failed to write {}", gitignore.display()))
}

/// Whether `line` is one of the entries earlier usagi versions appended to the
/// repository-root `.gitignore` (including the legacy bare `.usagi/` form). Such
/// lines are stripped on migration: a root `.usagi/` entry hides the directory
/// entirely, which would defeat the `.usagi/.gitignore` written above.
fn is_legacy_root_ignore_line(line: &str) -> bool {
    matches!(
        line.trim(),
        ".usagi"
            | ".usagi/"
            | ".usagi/*"
            | "!.usagi/issues"
            | "!.usagi/issues/"
            | ".usagi/issues/index.json"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::{issue_store, memory_store, store_lock};

    #[test]
    fn gitignore_covers_the_derived_and_lock_files() {
        // The ignore rules hardcode the derived-cache and lock filenames; if a
        // store renames its index or lock file, these constants change but the
        // literal string above would not — leaking the file into git. Assert the
        // string still mentions each actual filename so that drift is caught here.
        assert!(USAGI_GITIGNORE.contains(issue_store::INDEX_FILE));
        assert!(USAGI_GITIGNORE.contains(memory_store::INDEX_FILE));
        assert!(USAGI_GITIGNORE.contains(store_lock::LOCK_FILE_NAME));
    }

    #[test]
    fn write_usagi_gitignore_is_self_contained_and_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();

        write_usagi_gitignore(repo).unwrap();
        // A second run finds the file already current and leaves it untouched.
        write_usagi_gitignore(repo).unwrap();

        // The rules live inside .usagi/; the repo root is left clean.
        assert_eq!(
            fs::read_to_string(repo.join(".usagi/.gitignore")).unwrap(),
            USAGI_GITIGNORE
        );
        assert!(!repo.join(".gitignore").exists());
    }

    #[test]
    fn write_usagi_gitignore_reports_a_write_error() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        // A directory occupying .usagi/.gitignore makes the read return a
        // non-matching error and the subsequent write fail.
        fs::create_dir_all(repo.join(".usagi/.gitignore")).unwrap();
        assert!(write_usagi_gitignore(repo).is_err());
    }

    #[test]
    fn strip_migrates_legacy_root_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        // Earlier versions wrote usagi entries into the repo-root .gitignore.
        // They are stripped (a bare `.usagi/` would otherwise hide the whole
        // directory), while unrelated content and a clean ending are preserved.
        let block = "node_modules\n.usagi/*\n!.usagi/issues/\n.usagi/issues/index.json\n";
        for root in [block, "node_modules\n.usagi/\n\n"] {
            fs::write(repo.join(".gitignore"), root).unwrap();

            strip_legacy_root_entries(repo).unwrap();

            assert_eq!(
                fs::read_to_string(repo.join(".gitignore")).unwrap(),
                "node_modules\n"
            );
        }
    }

    #[test]
    fn strip_keeps_an_unrelated_root_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        fs::write(repo.join(".gitignore"), "target\n/build\n").unwrap();

        strip_legacy_root_entries(repo).unwrap();

        // No usagi lines to strip: the root file is left untouched.
        assert_eq!(
            fs::read_to_string(repo.join(".gitignore")).unwrap(),
            "target\n/build\n"
        );
    }

    #[test]
    fn strip_does_nothing_without_a_root_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        // No root .gitignore at all: a no-op that never creates one.
        strip_legacy_root_entries(repo).unwrap();
        assert!(!repo.join(".gitignore").exists());
    }

    #[test]
    fn strip_reports_a_root_read_error() {
        let tmp = tempfile::tempdir().unwrap();
        // A directory where the root .gitignore is expected fails to read with an
        // error other than NotFound, exercising that arm.
        fs::create_dir(tmp.path().join(".gitignore")).unwrap();
        assert!(strip_legacy_root_entries(tmp.path()).is_err());
    }
}

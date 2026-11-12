//! Git ignore rules for repository-local usagi metadata.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use super::paths::STATE_DIR;
use super::persistence::json_file::write_text_atomic;
use super::persistence::markdown_store::ensure_not_symlink;

/// Rules owned by `<repository>/.usagi/.gitignore`.
///
/// Memories are shared source files; derived indexes, locks,
/// session worktrees, and other daemon-local metadata are not.
pub const USAGI_GITIGNORE: &str =
    "/*\n!/.gitignore\n!/memory/\n/memory/index.json\n/memory/.derived-dirty\n/memory/.lock\n";

/// Install the self-contained usagi ignore file and migrate obsolete root rules.
///
/// This is idempotent and deliberately leaves unrelated root `.gitignore`
/// contents untouched.
///
/// # Errors
///
/// Returns an error when a metadata or root ignore file cannot be read, created,
/// or written.
pub fn migrate_usagi_ignore_rules(repo: &Path) -> Result<()> {
    let dir = repo.join(STATE_DIR);
    ensure_not_symlink(&dir)?;
    fs::create_dir_all(&dir).context(format!("failed to create {}", dir.display()))?;
    let gitignore = dir.join(".gitignore");
    ensure_not_symlink(&gitignore)?;
    if !fs::read_to_string(&gitignore).is_ok_and(|contents| contents == USAGI_GITIGNORE) {
        write_text_atomic(&gitignore, USAGI_GITIGNORE)?;
    }
    strip_legacy_root_entries(repo)
}

fn strip_legacy_root_entries(repo: &Path) -> Result<()> {
    let gitignore = repo.join(".gitignore");
    ensure_not_symlink(&gitignore)?;
    let existing = match fs::read_to_string(&gitignore) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context(format!("failed to read {}", gitignore.display())),
    };
    if !existing.lines().any(is_legacy_root_ignore_line) {
        return Ok(());
    }
    let mut kept: Vec<&str> = existing
        .lines()
        .filter(|line| !is_legacy_root_ignore_line(line))
        .collect();
    while kept.last().is_some_and(|line| line.trim().is_empty()) {
        kept.pop();
    }
    let mut output = String::new();
    for line in kept {
        output.push_str(line);
        output.push('\n');
    }
    write_text_atomic(&gitignore, &output)
}

/// Root rules written by older releases. The `.usagi/issues` entries belong to
/// the retired issue store and are still stripped so an upgraded checkout does
/// not keep re-including that directory.
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
    use std::fs;

    use super::{USAGI_GITIGNORE, migrate_usagi_ignore_rules};

    #[test]
    fn migration_writes_self_contained_rules_and_strips_only_legacy_root_rules() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join(".gitignore"),
            "target\n.usagi/*\n!.usagi/issues/\n.usagi/issues/index.json\n",
        )
        .unwrap();

        migrate_usagi_ignore_rules(tmp.path()).unwrap();
        migrate_usagi_ignore_rules(tmp.path()).unwrap();

        assert_eq!(
            fs::read_to_string(tmp.path().join(".usagi/.gitignore")).unwrap(),
            USAGI_GITIGNORE
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join(".gitignore")).unwrap(),
            "target\n"
        );
    }

    #[test]
    fn migration_keeps_current_rules_and_an_unrelated_root_ignore_file() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".usagi")).unwrap();
        fs::write(tmp.path().join(".usagi/.gitignore"), USAGI_GITIGNORE).unwrap();
        fs::write(tmp.path().join(".gitignore"), "target\n/build\n").unwrap();

        migrate_usagi_ignore_rules(tmp.path()).unwrap();

        assert_eq!(
            fs::read_to_string(tmp.path().join(".usagi/.gitignore")).unwrap(),
            USAGI_GITIGNORE
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join(".gitignore")).unwrap(),
            "target\n/build\n"
        );
    }

    #[test]
    fn migration_does_not_create_a_root_ignore_file_and_trims_legacy_blanks() {
        let tmp = tempfile::tempdir().unwrap();
        migrate_usagi_ignore_rules(tmp.path()).unwrap();
        assert!(!tmp.path().join(".gitignore").exists());

        fs::write(tmp.path().join(".gitignore"), "target\n.usagi/\n\n").unwrap();
        migrate_usagi_ignore_rules(tmp.path()).unwrap();

        assert_eq!(
            fs::read_to_string(tmp.path().join(".gitignore")).unwrap(),
            "target\n"
        );
    }

    #[test]
    fn migration_reports_an_unreadable_root_ignore_path() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".gitignore")).unwrap();

        assert!(migrate_usagi_ignore_rules(tmp.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn ignore_writers_refuse_symlinked_state_dir_and_gitignore_files() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        symlink(outside_dir.path(), tmp.path().join(".usagi")).unwrap();
        assert!(migrate_usagi_ignore_rules(tmp.path()).is_err());
        fs::remove_file(tmp.path().join(".usagi")).unwrap();

        let outside = tmp.path().join("outside");
        fs::write(&outside, "keep\n").unwrap();
        symlink(&outside, tmp.path().join(".gitignore")).unwrap();
        assert!(migrate_usagi_ignore_rules(tmp.path()).is_err());
        fs::remove_file(tmp.path().join(".gitignore")).unwrap();

        fs::remove_file(tmp.path().join(".usagi/.gitignore")).unwrap();
        symlink(&outside, tmp.path().join(".usagi/.gitignore")).unwrap();
        assert!(migrate_usagi_ignore_rules(tmp.path()).is_err());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep\n");
    }
}

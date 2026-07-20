//! Git ignore rules for repository-local usagi metadata.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use super::paths::STATE_DIR;

/// Rules owned by `<repository>/.usagi/.gitignore`.
///
/// Issues and memories are shared source files; derived indexes, locks,
/// session worktrees, and other daemon-local metadata are not.
pub const USAGI_GITIGNORE: &str = "/*\n!/.gitignore\n!/issues/\n/issues/index.json\n/issues/.derived-dirty\n/issues/.lock\n!/memory/\n/memory/index.json\n/memory/.derived-dirty\n/memory/.lock\n";

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
    fs::create_dir_all(&dir).context(format!("failed to create {}", dir.display()))?;
    let gitignore = dir.join(".gitignore");
    if !fs::read_to_string(&gitignore).is_ok_and(|contents| contents == USAGI_GITIGNORE) {
        fs::write(&gitignore, USAGI_GITIGNORE)
            .context(format!("failed to write {}", gitignore.display()))?;
    }
    strip_legacy_root_entries(repo)
}

fn strip_legacy_root_entries(repo: &Path) -> Result<()> {
    let gitignore = repo.join(".gitignore");
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
    fs::write(&gitignore, output).context(format!("failed to write {}", gitignore.display()))
}

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
}

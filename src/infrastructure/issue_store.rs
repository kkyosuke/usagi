//! Persistence for a repository's task issues.
//!
//! Issues live as `NNN-<slug>.md` files under `<repo>/.usagi/issues/`, each a
//! frontmatter markdown document (see [`crate::domain::issue`]). The markdown
//! files are the source of truth; `index.json` alongside them is a derived
//! cache of the metadata that speeds up listings and is rebuilt from the files
//! whenever it is missing or unreadable.
//!
//! Markdown files are meant to be committed and shared; `index.json` is a local
//! cache (kept out of git by `usagi init`'s `.gitignore` rules), so it is never
//! relied upon for correctness — only for speed.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::domain::issue::{Issue, IssueSummary};

const STATE_DIR_NAME: &str = ".usagi";
const ISSUES_DIR_NAME: &str = "issues";
const INDEX_FILE: &str = "index.json";
const FILE_FORMAT_VERSION: u32 = 1;

/// On-disk shape of `index.json`.
#[derive(Debug, Serialize, Deserialize)]
struct IndexFile {
    version: u32,
    issues: Vec<IssueSummary>,
}

/// File-based persistence rooted at a repository's `.usagi/issues/` directory.
pub struct IssueStore {
    dir: PathBuf,
}

impl IssueStore {
    /// Open the issue store for the repository at `repo_root`.
    pub fn new(repo_root: impl AsRef<Path>) -> Self {
        Self {
            dir: repo_root
                .as_ref()
                .join(STATE_DIR_NAME)
                .join(ISSUES_DIR_NAME),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn index_path(&self) -> PathBuf {
        self.dir.join(INDEX_FILE)
    }

    /// Read and parse every issue markdown file, sorted by number.
    pub fn scan(&self) -> Result<Vec<Issue>> {
        let mut issues = Vec::new();
        for path in self.issue_files()? {
            let text =
                fs::read_to_string(&path).context(format!("failed to read {}", path.display()))?;
            let issue = Issue::from_markdown(&text)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            issues.push(issue);
        }
        issues.sort_by_key(|i| i.number);
        Ok(issues)
    }

    /// Paths of every issue markdown file in the directory (the index and any
    /// non-`.md` files excluded). Empty when the directory does not exist.
    fn issue_files(&self) -> Result<Vec<PathBuf>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).context(format!("failed to read {}", self.dir.display())),
        };
        let mut files = Vec::new();
        for entry in entries {
            let path = entry
                .context(format!("failed to read an entry in {}", self.dir.display()))?
                .path();
            if is_issue_file(&path) {
                files.push(path);
            }
        }
        Ok(files)
    }

    /// The highest issue number currently stored, or 0 if there are none.
    pub fn max_number(&self) -> Result<u32> {
        Ok(self.scan()?.iter().map(|i| i.number).max().unwrap_or(0))
    }

    /// Read a single issue by number, or `None` if it does not exist.
    pub fn read(&self, number: u32) -> Result<Option<Issue>> {
        Ok(self.scan()?.into_iter().find(|i| i.number == number))
    }

    /// Write `issue` to disk and refresh the index.
    ///
    /// If a file for the same number already exists under a different name
    /// (because the title — and therefore the slug — changed), the stale file is
    /// removed so each issue is backed by exactly one file.
    pub fn write(&self, issue: &Issue) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .context(format!("failed to create {}", self.dir.display()))?;

        let target = self.dir.join(issue.file_name());
        for stale in self.files_for(issue.number)? {
            if stale != target {
                fs::remove_file(&stale).context(format!("failed to remove {}", stale.display()))?;
            }
        }

        write_atomically(&target, &issue.to_markdown())?;
        self.rebuild_index()?;
        Ok(())
    }

    /// Remove the issue with `number`, returning whether anything was deleted,
    /// then refresh the index.
    pub fn remove(&self, number: u32) -> Result<bool> {
        let files = self.files_for(number)?;
        if files.is_empty() {
            return Ok(false);
        }
        for file in files {
            fs::remove_file(&file).context(format!("failed to remove {}", file.display()))?;
        }
        self.rebuild_index()?;
        Ok(true)
    }

    /// Metadata summaries for every issue.
    ///
    /// Uses `index.json` when it is present and parseable; otherwise it rebuilds
    /// the index from the markdown files (self-healing on a missing or corrupt
    /// cache).
    pub fn summaries(&self) -> Result<Vec<IssueSummary>> {
        match self.load_index()? {
            Some(index) => Ok(index.issues),
            None => self.rebuild_index(),
        }
    }

    /// Load `index.json`, returning `None` when it is missing or unreadable (so
    /// the caller falls back to rebuilding from the markdown files).
    fn load_index(&self) -> Result<Option<IndexFile>> {
        let text = match fs::read_to_string(self.index_path()) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(e).context(format!("failed to read {}", self.index_path().display()))
            }
        };
        Ok(serde_json::from_str(&text).ok())
    }

    /// Rebuild `index.json` from the markdown files and return the summaries.
    fn rebuild_index(&self) -> Result<Vec<IssueSummary>> {
        let summaries: Vec<IssueSummary> = self.scan()?.iter().map(Issue::summary).collect();
        if summaries.is_empty() && !self.dir.exists() {
            // Nothing stored and no directory yet: don't create files eagerly.
            return Ok(summaries);
        }
        fs::create_dir_all(&self.dir)
            .context(format!("failed to create {}", self.dir.display()))?;
        let index = IndexFile {
            version: FILE_FORMAT_VERSION,
            issues: summaries.clone(),
        };
        let text = serde_json::to_string_pretty(&index)?;
        write_atomically(&self.index_path(), &format!("{text}\n"))?;
        Ok(summaries)
    }

    /// Every file that backs `number` (normally zero or one).
    fn files_for(&self, number: u32) -> Result<Vec<PathBuf>> {
        let prefix = format!("{number:03}-");
        Ok(self
            .issue_files()?
            .into_iter()
            .filter(|path| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|name| name.starts_with(&prefix))
            })
            .collect())
    }
}

/// Whether `path` is an issue markdown file (a `*.md` that is not the index).
fn is_issue_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("md")
}

/// Write `text` to `path` via a temp file + rename so a crash never leaves a
/// half-written file.
fn write_atomically(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text).context(format!("failed to write {}", tmp.display()))?;
    fs::rename(&tmp, path).context(format!("failed to replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::issue::{IssuePriority, IssueStatus};
    use chrono::{TimeZone, Utc};

    fn issue(number: u32, title: &str) -> Issue {
        let ts = Utc.with_ymd_and_hms(2026, 6, 14, 0, 0, 0).unwrap();
        Issue {
            number,
            title: title.to_string(),
            status: IssueStatus::Todo,
            priority: IssuePriority::Medium,
            labels: vec![],
            dependson: vec![],
            created_at: ts,
            updated_at: ts,
            body: format!("Body for {title}."),
        }
    }

    #[test]
    fn scan_is_empty_when_directory_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        assert!(store.scan().unwrap().is_empty());
        assert_eq!(store.max_number().unwrap(), 0);
    }

    #[test]
    fn dir_points_at_usagi_issues() {
        let store = IssueStore::new("/repo");
        assert_eq!(store.dir(), Path::new("/repo/.usagi/issues"));
        assert_eq!(
            store.index_path(),
            PathBuf::from("/repo/.usagi/issues/index.json")
        );
    }

    #[test]
    fn write_then_read_round_trips_and_writes_index() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        let i = issue(1, "First issue");

        store.write(&i).unwrap();

        assert!(tmp
            .path()
            .join(".usagi/issues/001-first-issue.md")
            .is_file());
        assert!(store.index_path().is_file());
        assert_eq!(store.read(1).unwrap().unwrap(), i);
        assert_eq!(store.max_number().unwrap(), 1);
    }

    #[test]
    fn index_records_the_format_version_and_summaries() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "First")).unwrap();

        let text = fs::read_to_string(store.index_path()).unwrap();
        assert!(text.contains("\"version\": 1"));
        assert!(text.contains("\"title\": \"First\""));
    }

    #[test]
    fn write_replaces_the_file_when_the_slug_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "Old title")).unwrap();
        assert!(tmp.path().join(".usagi/issues/001-old-title.md").is_file());

        let mut renamed = issue(1, "New title");
        renamed.body = "changed".to_string();
        store.write(&renamed).unwrap();

        // The stale file is gone and exactly one file backs the issue.
        assert!(!tmp.path().join(".usagi/issues/001-old-title.md").exists());
        assert!(tmp.path().join(".usagi/issues/001-new-title.md").is_file());
        assert_eq!(store.files_for(1).unwrap().len(), 1);
        assert_eq!(store.read(1).unwrap().unwrap().title, "New title");
    }

    #[test]
    fn remove_deletes_the_file_and_reports_success() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "Doomed")).unwrap();

        assert!(store.remove(1).unwrap());
        assert!(store.read(1).unwrap().is_none());
        // Removing again reports nothing was deleted.
        assert!(!store.remove(1).unwrap());
    }

    #[test]
    fn summaries_rebuild_when_index_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();
        store.write(&issue(2, "Two")).unwrap();

        // Delete the index; summaries should rebuild from the markdown files.
        fs::remove_file(store.index_path()).unwrap();
        let summaries = store.summaries().unwrap();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].number, 1);
        assert_eq!(summaries[1].number, 2);
        // Rebuilding re-creates the index file.
        assert!(store.index_path().is_file());
    }

    #[test]
    fn summaries_rebuild_when_index_is_corrupt() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();
        fs::write(store.index_path(), "{ not json").unwrap();

        let summaries = store.summaries().unwrap();
        assert_eq!(summaries.len(), 1);
        // The corrupt cache was replaced with a valid one.
        let text = fs::read_to_string(store.index_path()).unwrap();
        assert!(text.contains("\"version\": 1"));
    }

    #[test]
    fn summaries_are_empty_without_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        assert!(store.summaries().unwrap().is_empty());
        // No directory or index is created when there is nothing to store.
        assert!(!store.dir().exists());
    }

    #[test]
    fn scan_propagates_parse_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(store.dir().join("003-broken.md"), "not an issue").unwrap();

        let err = store.scan().unwrap_err();
        assert!(err.to_string().contains("failed to parse"));
    }

    #[test]
    fn scan_errors_when_the_issues_path_is_not_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        // A file where the issues directory should be makes read_dir fail with
        // a non-NotFound error.
        fs::create_dir_all(tmp.path().join(".usagi")).unwrap();
        fs::write(tmp.path().join(".usagi/issues"), "not a dir").unwrap();
        let store = IssueStore::new(tmp.path());

        assert!(store
            .scan()
            .unwrap_err()
            .to_string()
            .contains("failed to read"));
    }

    #[test]
    fn summaries_error_when_the_index_is_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();
        // Replace index.json with a directory so reading it fails with a
        // non-NotFound error rather than parsing to None.
        fs::remove_file(store.index_path()).unwrap();
        fs::create_dir(store.index_path()).unwrap();

        assert!(store
            .summaries()
            .unwrap_err()
            .to_string()
            .contains("failed to read"));
    }

    #[test]
    fn non_markdown_files_are_ignored_by_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();
        // A stray non-markdown file (and the index itself) must not be parsed.
        fs::write(store.dir().join("README.txt"), "ignore me").unwrap();

        assert_eq!(store.scan().unwrap().len(), 1);
    }
}

//! Persistence for a repository's task issues.
//!
//! Issues live as `NNN-<slug>.md` files under `<repo>/.usagi/issues/`, each a
//! frontmatter markdown document (see [`crate::domain::issue`]). The markdown
//! files are the source of truth; `index.json` alongside them is a derived
//! cache of the metadata that speeds up listings and is rebuilt from the files
//! whenever it is missing, unreadable, or stale relative to the markdown files.
//! Markdown files are meant to be committed and shared; `index.json` is a local
//! rebuildable cache, so it is never relied upon for correctness — only speed.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::domain::issue::{Issue, IssueSummary};
use crate::infrastructure::markdown_store::{MarkdownEntry, MarkdownStore, MutationOutcome};
use crate::infrastructure::repo_paths::STATE_DIR;
use crate::infrastructure::store_lock::StoreLock;

const ISSUES_DIR_NAME: &str = "issues";

/// More than one markdown file claims the same issue number.
///
/// The exact paths are retained so callers can downcast the `anyhow::Error` and
/// present a deterministic repair plan without guessing which sibling is real.
#[derive(Debug)]
pub struct AmbiguousIssueNumber {
    pub number: u32,
    pub files: Vec<PathBuf>,
}

impl fmt::Display for AmbiguousIssueNumber {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "issue #{} is ambiguous; refusing to choose among these files: {}",
            self.number,
            self.files
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

impl std::error::Error for AmbiguousIssueNumber {}

#[derive(Debug, Deserialize)]
struct IndexFile {
    issues: Vec<IssueSummary>,
}

#[derive(Serialize)]
struct IndexFileRef<'a> {
    version: u32,
    issues: &'a [IssueSummary],
}

struct IssueEntry;

impl MarkdownEntry for IssueEntry {
    type Entry = Issue;
    type Summary = IssueSummary;
    type Key = u32;
    type IndexFile = IndexFile;
    type IndexFileRef<'a> = IndexFileRef<'a>;

    const NAME: &'static str = "issue";

    fn is_entry_file(path: &Path) -> bool {
        is_issue_file(path)
    }

    fn parse_markdown(text: &str) -> Result<Issue> {
        Ok(Issue::from_markdown(text)?)
    }

    fn to_markdown(entry: &Issue) -> String {
        entry.to_markdown()
    }

    fn file_name(entry: &Issue) -> Result<String> {
        Ok(entry.file_name())
    }

    fn key(entry: &Issue) -> u32 {
        entry.number
    }

    fn key_from_summary(summary: &IssueSummary) -> u32 {
        summary.number
    }

    fn key_from_path(path: &Path) -> Option<u32> {
        number_from_filename(path)
    }

    fn summary(entry: &Issue) -> IssueSummary {
        entry.summary()
    }

    fn sort_entries(entries: &mut Vec<Issue>) {
        entries.sort_by_key(|i| i.number);
    }

    fn summaries_from_index(index: IndexFile) -> Vec<IssueSummary> {
        index.issues
    }

    fn index_file_ref(summaries: &[IssueSummary]) -> IndexFileRef<'_> {
        IndexFileRef {
            version: crate::infrastructure::json_file::FILE_FORMAT_VERSION,
            issues: summaries,
        }
    }
}

/// File-based persistence rooted at a repository's `.usagi/issues/` directory.
pub struct IssueStore {
    inner: MarkdownStore<IssueEntry>,
}

impl IssueStore {
    /// Open the issue store for the repository at `repo_root`.
    pub fn new(repo_root: impl AsRef<Path>) -> Self {
        Self {
            inner: MarkdownStore::new(repo_root.as_ref().join(STATE_DIR).join(ISSUES_DIR_NAME)),
        }
    }

    pub fn dir(&self) -> &Path {
        self.inner.dir()
    }

    pub fn index_path(&self) -> PathBuf {
        self.inner.index_path()
    }

    /// Acquire this store's cross-process write lock, blocking until it is free.
    /// Hold the guard across read-modify-write operations that must be atomic,
    /// such as allocating the next issue number and writing the issue.
    pub fn lock(&self) -> Result<StoreLock> {
        StoreLock::acquire(self.dir())
    }

    /// Read and parse every issue markdown file, sorted by number.
    pub fn scan(&self) -> Result<Vec<Issue>> {
        self.inner.scan()
    }

    /// Like [`scan`](Self::scan), but logs unreadable/unparseable issue files and
    /// skips them so one corrupt sibling cannot break listings or cache rebuilds.
    pub fn scan_lenient(&self) -> Result<Vec<Issue>> {
        self.inner.scan_lenient()
    }

    /// Paths of every issue markdown file. Empty when the directory is missing.
    fn issue_files(&self) -> Result<Vec<PathBuf>> {
        self.inner.entry_files()
    }

    /// The highest issue number currently stored, or 0 if there are none.
    pub fn max_number(&self) -> Result<u32> {
        Ok(self
            .issue_files()?
            .iter()
            .filter_map(|path| number_from_filename(path))
            .max()
            .unwrap_or(0))
    }

    /// Read a single issue by number, or `None` if it does not exist.
    pub fn read(&self, number: u32) -> Result<Option<Issue>> {
        self.repair_derived_best_effort();
        self.read_locked(number)
    }

    /// Read source while the caller already holds this store's lock.
    pub fn read_locked(&self, number: u32) -> Result<Option<Issue>> {
        let files = self.unique_files_for(number)?;
        let Some(path) = files.into_iter().next() else {
            return Ok(None);
        };
        Ok(Some(self.inner.read_existing_path(&path)?))
    }

    /// Write `issue` to disk and refresh the index, taking the store lock for the
    /// duration so concurrent writers serialise.
    pub fn write(&self, issue: &Issue) -> Result<MutationOutcome<()>> {
        let lock = self.lock()?;
        self.write_locked(&lock, issue)
    }

    /// Like [`write`](Self::write) but assumes the caller already holds this
    /// store's [`lock`](Self::lock). If the title changes, the new file is written
    /// before stale-named siblings for the same number are removed.
    pub fn write_locked(&self, _lock: &StoreLock, issue: &Issue) -> Result<MutationOutcome<()>> {
        let rebuild_required = self.inner.derived_is_dirty();
        self.inner.mark_derived_dirty()?;
        let existing = self.unique_files_for(issue.number)?;
        let target = self.inner.write_markdown(issue)?;
        for stale in existing {
            if stale != target {
                fs::remove_file(&stale).context(format!("failed to remove {}", stale.display()))?;
            }
        }
        let refresh = if rebuild_required {
            self.inner.rebuild_derived().map(|_| ())
        } else {
            self.inner.reindex_after_write(issue)
        };
        Ok(self.inner.finish_committed((), refresh))
    }

    /// Remove the issue with `number`, returning whether anything was deleted,
    /// then refresh the index. Takes the store lock for the duration.
    pub fn remove(&self, number: u32) -> Result<bool> {
        Ok(self.remove_with_outcome(number)?.value)
    }

    /// Remove source and report whether its derived files remain fresh.
    pub fn remove_with_outcome(&self, number: u32) -> Result<MutationOutcome<bool>> {
        let _lock = self.lock()?;
        let files = self.unique_files_for(number)?;
        if files.is_empty() {
            let repair = self.inner.repair_derived_locked();
            return Ok(self.inner.finish_committed(false, repair));
        }
        let rebuild_required = self.inner.derived_is_dirty();
        self.inner.mark_derived_dirty()?;
        for file in files {
            fs::remove_file(&file).context(format!("failed to remove {}", file.display()))?;
        }
        let refresh = if rebuild_required {
            self.inner.rebuild_derived().map(|_| ())
        } else {
            self.inner.reindex_after_remove(&number)
        };
        Ok(self.inner.finish_committed(true, refresh))
    }

    /// Metadata summaries for every issue.
    pub fn summaries(&self) -> Result<Vec<IssueSummary>> {
        self.repair_derived_best_effort();
        if self.inner.derived_is_dirty() {
            return self.inner.source_summaries();
        }
        self.inner.summaries()
    }

    fn repair_derived_best_effort(&self) {
        if !self.inner.derived_is_dirty() {
            return;
        }
        let repair = self
            .lock()
            .and_then(|_lock| self.inner.repair_derived_locked());
        if let Err(error) = repair {
            crate::infrastructure::error_log::ErrorLog::record(&format!(
                "issue derived rebuild remains scheduled after read: {error:#}"
            ));
        }
    }

    /// Write the number-sorted `summaries` to `index.json` as the derived cache.
    #[cfg(test)]
    fn write_index(&self, summaries: &[IssueSummary]) -> Result<()> {
        self.inner.write_index(summaries)
    }

    /// Every file that backs `number` (normally zero or one).
    fn files_for(&self, number: u32) -> Result<Vec<PathBuf>> {
        self.inner.files_for_key(&number)
    }

    fn unique_files_for(&self, number: u32) -> Result<Vec<PathBuf>> {
        let mut files = self.files_for(number)?;
        files.sort();
        if files.len() > 1 {
            return Err(AmbiguousIssueNumber { number, files }.into());
        }
        Ok(files)
    }
}

/// Whether `path` is an issue markdown file.
fn is_issue_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("md")
}

/// The issue number encoded in an issue file's name (`NNN-slug.md`), or `None`
/// when the name has no numeric prefix.
fn number_from_filename(path: &Path) -> Option<u32> {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.split_once('-'))
        .and_then(|(number, _)| number.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::issue::{IssuePriority, IssueStatus};
    use crate::infrastructure::json_file::{fail_next_atomic_write, AtomicWriteStage};
    use crate::infrastructure::markdown_store::DerivedState;
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
            related: vec![],
            parent: None,
            milestone: None,
            created_at: ts,
            updated_at: ts,
            body: format!("Body for {title}."),
        }
    }

    /// Pin a file's modification time so freshness tests are independent of the
    /// filesystem's timestamp granularity.
    fn set_mtime(path: &Path, secs_from_epoch: u64) {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs_from_epoch);
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
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
    fn duplicate_number_read_write_and_remove_fail_closed_with_exact_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        fs::create_dir_all(store.dir()).unwrap();
        let first = store.dir().join("007-first.md");
        let second = store.dir().join("007-second.md");
        fs::write(&first, issue(7, "First").to_markdown()).unwrap();
        fs::write(&second, issue(7, "Second").to_markdown()).unwrap();

        for error in [
            store.read(7).unwrap_err(),
            store.write(&issue(7, "Replacement")).unwrap_err(),
            store.remove(7).unwrap_err(),
        ] {
            let message = error.to_string();
            assert!(message.contains("issue #7 is ambiguous"));
            assert!(message.contains(first.to_str().unwrap()));
            assert!(message.contains(second.to_str().unwrap()));
            let ambiguity = error.downcast_ref::<AmbiguousIssueNumber>().unwrap();
            assert_eq!(ambiguity.number, 7);
            assert_eq!(ambiguity.files, vec![first.clone(), second.clone()]);
        }

        assert_eq!(
            fs::read_to_string(&first).unwrap(),
            issue(7, "First").to_markdown()
        );
        assert_eq!(
            fs::read_to_string(&second).unwrap(),
            issue(7, "Second").to_markdown()
        );
        assert!(!store.dir().join("007-replacement.md").exists());
        assert!(!store.index_path().exists());
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

        assert!(!tmp.path().join(".usagi/issues/001-old-title.md").exists());
        assert!(tmp.path().join(".usagi/issues/001-new-title.md").is_file());
        assert_eq!(store.files_for(1).unwrap().len(), 1);
        assert_eq!(store.read(1).unwrap().unwrap().title, "New title");
    }

    #[test]
    fn files_for_matches_any_numeric_prefix_not_just_zero_padded() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "First")).unwrap();

        let dir = store.dir().to_path_buf();
        fs::rename(dir.join("001-first.md"), dir.join("1-first.md")).unwrap();

        assert_eq!(store.files_for(1).unwrap().len(), 1);
        assert_eq!(store.read(1).unwrap().unwrap().title, "First");
    }

    #[test]
    fn remove_deletes_the_file_and_reports_success() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "Doomed")).unwrap();

        assert!(store.remove(1).unwrap());
        assert!(store.read(1).unwrap().is_none());
        assert!(!store.remove(1).unwrap());
    }

    #[test]
    fn remove_rebuilds_the_index_when_the_cache_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();
        store.write(&issue(2, "Two")).unwrap();
        fs::remove_file(store.index_path()).unwrap();

        assert!(store.remove(1).unwrap());

        let nums: Vec<u32> = store
            .summaries()
            .unwrap()
            .iter()
            .map(|s| s.number)
            .collect();
        assert_eq!(nums, vec![2]);
        assert!(store.index_path().is_file());
    }

    #[test]
    fn remove_leaves_the_cache_untouched_when_the_number_is_absent_from_it() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(2, "Two")).unwrap();
        fs::write(
            store.dir().join("001-one.md"),
            issue(1, "One").to_markdown(),
        )
        .unwrap();

        assert!(store.remove(1).unwrap());
        let nums: Vec<u32> = store
            .summaries()
            .unwrap()
            .iter()
            .map(|s| s.number)
            .collect();
        assert_eq!(nums, vec![2]);
    }

    #[test]
    fn summaries_rebuild_when_index_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();
        store.write(&issue(2, "Two")).unwrap();

        fs::remove_file(store.index_path()).unwrap();
        let summaries = store.summaries().unwrap();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].number, 1);
        assert_eq!(summaries[1].number, 2);
        assert!(store.index_path().is_file());
    }

    #[test]
    fn summaries_rebuild_when_index_is_corrupt() {
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, home.path());

        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();
        fs::write(store.index_path(), "{ not json").unwrap();

        let summaries = store.summaries().unwrap();
        assert_eq!(summaries.len(), 1);
        let text = fs::read_to_string(store.index_path()).unwrap();
        assert!(text.contains("\"version\": 1"));

        let entry = fs::read_dir(home.path().join("logs"))
            .expect("logs dir exists")
            .next()
            .expect("a log file was written")
            .expect("readable entry");
        assert!(fs::read_to_string(entry.path())
            .unwrap()
            .contains("is corrupt"));

        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn summaries_are_empty_without_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        assert!(store.summaries().unwrap().is_empty());
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
    fn write_tolerates_a_corrupt_sibling_and_indexes_the_parseable_files() {
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, home.path());

        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "First")).unwrap();
        fs::write(store.dir().join("002-broken.md"), "not an issue").unwrap();

        store.write(&issue(3, "Third")).unwrap();
        let nums: Vec<u32> = store
            .summaries()
            .unwrap()
            .iter()
            .map(|s| s.number)
            .collect();
        assert_eq!(nums, vec![1, 3]);

        fs::remove_file(store.index_path()).unwrap();
        let rebuilt: Vec<u32> = store
            .summaries()
            .unwrap()
            .iter()
            .map(|s| s.number)
            .collect();
        assert_eq!(rebuilt, vec![1, 3]);
        assert!(store.scan().is_err());

        let entry = fs::read_dir(home.path().join("logs"))
            .expect("logs dir exists")
            .next()
            .expect("a log file was written")
            .expect("readable entry");
        assert!(fs::read_to_string(entry.path())
            .unwrap()
            .contains("skipping unparseable issue file"));

        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn scan_errors_when_the_issues_path_is_not_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
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
        fs::remove_file(store.index_path()).unwrap();
        fs::create_dir(store.index_path()).unwrap();

        assert!(store
            .summaries()
            .unwrap_err()
            .to_string()
            .contains("failed to read"));
    }

    #[test]
    fn read_propagates_parse_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(store.dir().join("003-broken.md"), "not an issue").unwrap();

        let err = store.read(3).unwrap_err();
        assert!(err.to_string().contains("failed to parse"));
    }

    #[test]
    fn read_errors_when_the_backing_file_is_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        fs::create_dir_all(store.dir()).unwrap();
        fs::create_dir(store.dir().join("003-broken.md")).unwrap();

        assert!(store
            .read(3)
            .unwrap_err()
            .to_string()
            .contains("failed to read"));
    }

    #[test]
    fn max_number_reflects_files_added_outside_usagi() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();
        fs::write(
            store.dir().join("002-two.md"),
            issue(2, "Two").to_markdown(),
        )
        .unwrap();

        assert_eq!(store.max_number().unwrap(), 2);
    }

    #[test]
    fn creating_the_next_issue_does_not_clobber_a_file_missing_from_the_index() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();
        fs::write(
            store.dir().join("002-two.md"),
            issue(2, "Two").to_markdown(),
        )
        .unwrap();

        let next = store.max_number().unwrap() + 1;
        store.write(&issue(next, "Three")).unwrap();

        assert_eq!(next, 3);
        assert!(store.dir().join("002-two.md").exists());
        assert_eq!(store.scan().unwrap().len(), 3);
    }

    #[test]
    fn write_renames_in_place_leaving_one_valid_file_and_a_fresh_index() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "Old title")).unwrap();

        let mut renamed = issue(1, "New title");
        renamed.body = "changed".to_string();
        store.write(&renamed).unwrap();

        assert!(!store.dir().join("001-old-title.md").exists());
        assert!(store.dir().join("001-new-title.md").is_file());
        assert_eq!(store.files_for(1).unwrap().len(), 1);
        assert_eq!(store.read(1).unwrap().unwrap().title, "New title");
        let index = fs::read_to_string(store.index_path()).unwrap();
        assert!(index.contains("\"title\": \"New title\""));
        assert!(!index.contains("Old title"));
    }

    #[test]
    fn the_lock_file_is_not_picked_up_as_an_issue() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();
        let _guard = store.lock().unwrap();
        assert!(store.dir().join(".lock").is_file());
        assert_eq!(store.scan().unwrap().len(), 1);
    }

    #[test]
    fn summaries_rebuild_when_an_issue_file_is_newer_than_the_index() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();

        let mut edited = issue(1, "One");
        edited.status = IssueStatus::Done;
        let path = store.dir().join("001-one.md");
        fs::write(&path, edited.to_markdown()).unwrap();

        set_mtime(&store.index_path(), 1_000);
        set_mtime(&path, 2_000);

        assert_eq!(store.summaries().unwrap()[0].status, IssueStatus::Done);
    }

    #[test]
    fn summaries_trust_a_fresh_index_without_rereading_the_markdown() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();

        let mut cached = issue(1, "One").summary();
        cached.title = "Cached title".to_string();
        store.write_index(&[cached]).unwrap();

        set_mtime(&store.dir().join("001-one.md"), 1_000);
        set_mtime(&store.index_path(), 2_000);

        assert_eq!(store.summaries().unwrap()[0].title, "Cached title");
    }

    #[test]
    fn summaries_rebuild_when_a_file_is_added_outside_usagi() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();
        fs::write(
            store.dir().join("002-two.md"),
            issue(2, "Two").to_markdown(),
        )
        .unwrap();

        set_mtime(&store.dir().join("001-one.md"), 1_000);
        set_mtime(&store.dir().join("002-two.md"), 1_000);
        set_mtime(&store.index_path(), 2_000);

        let nums: Vec<u32> = store
            .summaries()
            .unwrap()
            .iter()
            .map(|s| s.number)
            .collect();
        assert_eq!(nums, vec![1, 2]);
    }

    #[test]
    fn summaries_rebuild_when_a_file_is_removed_outside_usagi() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();
        store.write(&issue(2, "Two")).unwrap();
        fs::remove_file(store.dir().join("002-two.md")).unwrap();

        set_mtime(&store.dir().join("001-one.md"), 1_000);
        set_mtime(&store.index_path(), 2_000);

        let nums: Vec<u32> = store
            .summaries()
            .unwrap()
            .iter()
            .map(|s| s.number)
            .collect();
        assert_eq!(nums, vec![1]);
    }

    #[test]
    fn non_markdown_files_are_ignored_by_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();
        fs::write(store.dir().join("README.txt"), "ignore me").unwrap();

        assert_eq!(store.scan().unwrap().len(), 1);
    }

    #[test]
    fn derived_failure_commits_issue_mutations_and_reopen_repairs_them() {
        let _guard = crate::test_support::process_env_guard();
        let logs = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, logs.path());
        for stage in [AtomicWriteStage::Write, AtomicWriteStage::Rename] {
            let tmp = tempfile::tempdir().unwrap();
            let store = IssueStore::new(tmp.path());
            let created = issue(1, "Created");
            fail_next_atomic_write(&store.index_path(), stage);

            let outcome = store.write(&created).unwrap();
            assert_eq!(outcome.derived, DerivedState::RebuildNeeded);
            assert_eq!(store.scan().unwrap(), vec![created.clone()]);

            let updated = issue(1, "Updated");
            let reopened = IssueStore::new(tmp.path());
            assert_eq!(reopened.read(1).unwrap(), Some(created));
            fail_next_atomic_write(&reopened.index_path(), stage);
            assert_eq!(
                reopened.write(&updated).unwrap().derived,
                DerivedState::RebuildNeeded
            );
            assert_eq!(reopened.read(1).unwrap(), Some(updated));

            fail_next_atomic_write(&reopened.index_path(), stage);
            let removed = reopened.remove_with_outcome(1).unwrap();
            assert!(removed.value);
            assert_eq!(removed.derived, DerivedState::RebuildNeeded);
            let retry = IssueStore::new(tmp.path()).remove_with_outcome(1).unwrap();
            assert!(!retry.value);
            assert_eq!(retry.derived, DerivedState::Fresh);
        }
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn source_failure_does_not_mutate_issue_identity() {
        for stage in [AtomicWriteStage::Write, AtomicWriteStage::Rename] {
            let tmp = tempfile::tempdir().unwrap();
            let store = IssueStore::new(tmp.path());
            let created_path = store.dir().join("001-created.md");
            fail_next_atomic_write(&created_path, stage);
            assert!(store.write(&issue(1, "Created")).is_err());
            assert!(store.scan().unwrap().is_empty());

            store.write(&issue(1, "Old")).unwrap();
            let updated_path = store.dir().join("001-updated.md");
            fail_next_atomic_write(&updated_path, stage);
            assert!(store.write(&issue(1, "Updated")).is_err());
            assert_eq!(store.read(1).unwrap().unwrap().title, "Old");
        }
    }
}

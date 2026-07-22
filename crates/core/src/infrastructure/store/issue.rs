//! Persistence for a repository's task issues.
//!
//! Issues live as `NNN-<slug>.md` files under `<repo>/.usagi/issues/`, each a
//! frontmatter markdown document (see [`crate::domain::issue`]). The markdown
//! files are the source of truth; `index.json` alongside them is a derived
//! cache of the metadata that speeds up listings and is rebuilt from the files
//! whenever it is missing, unreadable, or stale relative to the markdown files.
//! Markdown files are meant to be committed and shared; `index.json` is a local
//! rebuildable cache, so it is never relied upon for correctness — only speed.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::domain::frontmatter::FrontmatterDoc;
use crate::domain::issue::{Issue, IssueSummary};
use crate::infrastructure::error_log::ErrorLog;
use crate::infrastructure::paths::STATE_DIR;
use crate::infrastructure::persistence::json_file::write_text_atomic;
use crate::infrastructure::persistence::markdown_store::{MarkdownEntry, MarkdownStore};
use crate::infrastructure::persistence::store_lock::StoreLock;
use crate::infrastructure::store::MutationOutcome;

const ISSUES_DIR_NAME: &str = "issues";
const ALLOCATION_DIR_NAME: &str = "usagi-issue-sequence";
const ALLOCATION_FILE_NAME: &str = "next";

/// More than one Markdown source file claims the same issue number.
///
/// Exact paths are retained in deterministic order so callers can present a
/// repair plan without guessing which sibling is authoritative.
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// A parseable Markdown source disagrees with the issue number claimed by its
/// filename. Point operations refuse to reinterpret either number as the
/// authoritative identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MismatchedIssueNumber {
    pub filename_number: u32,
    pub declared_number: u32,
    pub file: PathBuf,
}

impl fmt::Display for MismatchedIssueNumber {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "issue source {} declares #{} but its filename claims #{}; refusing point operation",
            self.file.display(),
            self.declared_number,
            self.filename_number
        )
    }
}

impl std::error::Error for MismatchedIssueNumber {}

#[derive(Debug, Deserialize)]
struct IndexFile {
    version: u32,
    source_fingerprint: String,
    issues: Vec<IssueSummary>,
}

#[derive(Serialize)]
struct IndexFileRef<'a> {
    version: u32,
    source_fingerprint: &'a str,
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

    fn key_from_path(path: &Path) -> Option<u32> {
        number_from_filename(path)
    }

    fn summary(entry: &Issue) -> IssueSummary {
        entry.summary()
    }

    fn sort_entries(entries: &mut Vec<Issue>) {
        entries.sort_by_key(|i| i.number);
    }

    fn index_parts(index: IndexFile) -> (u32, String, Vec<IssueSummary>) {
        (index.version, index.source_fingerprint, index.issues)
    }

    fn index_file_ref<'a>(
        summaries: &'a [IssueSummary],
        source_fingerprint: &'a str,
    ) -> IndexFileRef<'a> {
        IndexFileRef {
            version: crate::infrastructure::persistence::markdown_store::INDEX_FORMAT_VERSION,
            source_fingerprint,
            issues: summaries,
        }
    }
}

/// File-based persistence rooted at a repository's `.usagi/issues/` directory.
pub struct IssueStore {
    inner: MarkdownStore<IssueEntry>,
    repo_root: PathBuf,
}

/// A parseable issue paired with the exact Markdown file that supplied it.
pub(crate) struct IssueSource {
    pub issue: Issue,
    pub file: String,
    pub path: PathBuf,
    pub filename_number: Option<u32>,
}

impl IssueSource {
    pub fn summary(self) -> IssueSummary {
        let mut summary = self.issue.summary();
        summary.file = self.file;
        summary
    }
}

/// One lock-protected view of every filename claim and every parseable source.
/// Corrupt sources remain in `claims`, so they can block readiness even though
/// they cannot produce a summary row.
#[derive(Default)]
pub(crate) struct IssueSourceSnapshot {
    pub sources: Vec<IssueSource>,
    pub claims: BTreeMap<u32, Vec<PathBuf>>,
}

impl IssueStore {
    /// Open the issue store for the repository at `repo_root`.
    #[must_use]
    pub fn new(repo_root: impl AsRef<Path>) -> Self {
        let repo_root = repo_root.as_ref().to_path_buf();
        Self {
            inner: MarkdownStore::new(repo_root.join(STATE_DIR).join(ISSUES_DIR_NAME)),
            repo_root,
        }
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        self.inner.dir()
    }

    #[must_use]
    pub fn index_path(&self) -> PathBuf {
        self.inner.index_path()
    }

    /// Acquire this store's cross-process write lock, blocking until it is free.
    /// Hold the guard across read-modify-write operations that must be atomic,
    /// such as allocating the next issue number and writing the issue.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock cannot be acquired.
    pub fn lock(&self) -> Result<StoreLock> {
        StoreLock::acquire(self.dir())
    }

    /// Read and parse every issue markdown file, sorted by number.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory cannot be read or any file fails to
    /// parse.
    pub fn scan(&self) -> Result<Vec<Issue>> {
        self.inner.scan()
    }

    /// Like [`scan`](Self::scan), but logs unreadable/unparseable issue files and
    /// skips them so one corrupt sibling cannot break listings or cache rebuilds.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory itself cannot be read.
    pub fn scan_lenient(&self) -> Result<Vec<Issue>> {
        Ok(self
            .scan_sources_lenient()?
            .into_iter()
            .map(|source| source.issue)
            .collect())
    }

    /// Paths of every issue markdown file. Empty when the directory is missing.
    fn issue_files(&self) -> Result<Vec<PathBuf>> {
        self.inner.entry_files()
    }

    /// Parse source files leniently while retaining their exact filenames.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory cannot be read or a source filename
    /// cannot be represented as UTF-8. Individual read and parse failures are
    /// logged and skipped.
    pub(crate) fn scan_sources_lenient(&self) -> Result<Vec<IssueSource>> {
        Ok(self.snapshot_from_files(self.issue_files()?)?.sources)
    }

    /// Capture filename claims and parseable sources from one directory
    /// enumeration while holding the issue store lock.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock or directory cannot be read, or a source
    /// filename cannot be represented as UTF-8.
    pub(crate) fn source_snapshot(&self) -> Result<IssueSourceSnapshot> {
        self.source_snapshot_with_hook(|| {})
    }

    fn source_snapshot_with_hook(&self, after_lock: impl FnOnce()) -> Result<IssueSourceSnapshot> {
        if !self.source_dir_exists()? {
            return Ok(IssueSourceSnapshot::default());
        }
        let lock = self.lock()?;
        after_lock();
        self.repair_derived_best_effort_locked(&lock);
        self.source_snapshot_locked(&lock)
    }

    #[cfg(test)]
    pub(crate) fn source_snapshot_after_lock_for_test(
        &self,
        after_lock: impl FnOnce(),
    ) -> Result<IssueSourceSnapshot> {
        self.source_snapshot_with_hook(after_lock)
    }

    /// Capture a source snapshot while the caller already holds this store's
    /// lock, avoiding nested acquisition in create retry validation.
    pub(crate) fn source_snapshot_locked(&self, _lock: &StoreLock) -> Result<IssueSourceSnapshot> {
        self.snapshot_from_files(self.issue_files()?)
    }

    fn snapshot_from_files(&self, mut files: Vec<PathBuf>) -> Result<IssueSourceSnapshot> {
        files.sort();
        let mut claims = BTreeMap::<u32, Vec<PathBuf>>::new();
        let mut sources = Vec::new();
        for path in files {
            let filename_number = number_from_filename(&path);
            if let Some(number) = filename_number {
                claims.entry(number).or_default().push(path.clone());
            }
            let file = path
                .file_name()
                .and_then(|name| name.to_str())
                .context(format!("source filename is not UTF-8: {}", path.display()))?
                .to_owned();
            match self.inner.read_existing_path(&path) {
                Ok(issue) => sources.push(IssueSource {
                    issue,
                    file,
                    path,
                    filename_number,
                }),
                Err(error) => ErrorLog::record(&format!(
                    "skipping unparseable issue file {}: {error:#}",
                    path.display()
                )),
            }
        }
        sources.sort_by(|left, right| {
            left.issue
                .number
                .cmp(&right.issue.number)
                .then_with(|| left.path.cmp(&right.path))
        });
        Ok(IssueSourceSnapshot { sources, claims })
    }

    /// The highest issue number currently stored, or 0 if there are none.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory cannot be read.
    pub fn max_number(&self) -> Result<u32> {
        Ok(self
            .issue_files()?
            .iter()
            .filter_map(|path| number_from_filename(path))
            .max()
            .unwrap_or(0))
    }

    /// Reserve the next issue number across every worktree of this repository.
    ///
    /// A worktree has its own checked-out `.usagi/issues` directory, so its
    /// local maximum alone cannot safely allocate a number while another
    /// worktree is creating an issue. The git common directory is shared by
    /// every worktree; a locked sequence there makes each reservation unique.
    /// The local maximum is folded in to migrate repositories that predate the
    /// sequence file or received an issue markdown file manually.
    ///
    /// # Errors
    ///
    /// Returns an error when the allocation lock or sequence cannot be read or
    /// written.
    pub fn reserve_next_number(&self) -> Result<u32> {
        let allocation_dir = self.allocation_dir()?;
        let _lock = StoreLock::acquire(&allocation_dir)?;
        let sequence_path = allocation_dir.join(ALLOCATION_FILE_NAME);
        let reserved = match fs::read_to_string(&sequence_path) {
            Ok(text) => text.trim().parse::<u32>().context(format!(
                "invalid issue sequence in {}",
                sequence_path.display()
            ))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => {
                return Err(error).context(format!(
                    "failed to read issue sequence {}",
                    sequence_path.display()
                ));
            }
        };
        let next = reserved
            .max(self.max_number()?)
            .checked_add(1)
            .context("cannot allocate another issue number because the u32 range is exhausted")?;
        write_text_atomic(&sequence_path, &format!("{next}\n"))?;
        Ok(next)
    }

    /// Read a single issue by number, or `None` if it does not exist.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory cannot be read or the backing file
    /// cannot be read or parsed, or when more than one source file claims
    /// `number`.
    pub fn read(&self, number: u32) -> Result<Option<Issue>> {
        if !self.source_dir_exists()? {
            return Ok(None);
        }
        let lock = self.lock()?;
        let path = self.unique_path_for(number)?;
        self.repair_derived_best_effort_locked(&lock);
        path.map(|path| self.read_numbered_path(number, &path))
            .transpose()
    }

    /// Read source while the caller already holds the store lock.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory or source file cannot be read, or
    /// when more than one source file claims `number`.
    pub fn read_locked(&self, number: u32) -> Result<Option<Issue>> {
        self.unique_path_for(number)?
            .map(|path| self.read_numbered_path(number, &path))
            .transpose()
    }

    /// Write `issue` to disk and refresh the index, taking the store lock for the
    /// duration so concurrent writers serialise.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock cannot be acquired, the issue number is
    /// ambiguous, or source cannot be committed. A derived refresh failure is
    /// returned in the outcome.
    pub fn write(&self, issue: &Issue) -> Result<MutationOutcome<()>> {
        let lock = self.lock()?;
        self.write_locked(&lock, issue)
    }

    /// Like [`write`](Self::write) but assumes the caller already holds this
    /// store's [`lock`](Self::lock). If the title changes, the new file is written
    /// before the one stale-named existing file is removed.
    ///
    /// # Errors
    ///
    /// Returns an error when the number is ambiguous, the markdown cannot be
    /// written, the stale name cannot be removed, or the dirty marker cannot be
    /// scheduled. Ambiguity is detected before any source or derived mutation.
    pub fn write_locked(&self, _lock: &StoreLock, issue: &Issue) -> Result<MutationOutcome<()>> {
        let existing = self.unique_files_for(issue.number)?;
        self.ensure_existing_number_matches(issue.number, &existing)?;
        let rebuild_required = self.inner.derived_is_dirty();
        self.inner.mark_derived_dirty()?;
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
    ///
    /// # Errors
    ///
    /// Returns an error when the lock cannot be acquired, the issue number is
    /// ambiguous, or source cannot be removed. A derived refresh failure does
    /// not change the returned delete.
    pub fn remove(&self, number: u32) -> Result<bool> {
        Ok(self.remove_with_outcome(number)?.value)
    }

    /// Remove an issue and report whether committed source left derived files
    /// fresh or scheduled for rebuild.
    ///
    /// # Errors
    ///
    /// Returns an error only before the source removal commits, including when
    /// more than one source file claims `number`.
    pub fn remove_with_outcome(&self, number: u32) -> Result<MutationOutcome<bool>> {
        let _lock = self.lock()?;
        let files = self.unique_files_for(number)?;
        self.ensure_existing_number_matches(number, &files)?;
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
    ///
    /// # Errors
    ///
    /// Returns an error when the index cannot be read and the markdown source
    /// cannot be rescanned.
    pub fn summaries(&self) -> Result<Vec<IssueSummary>> {
        self.repair_derived_best_effort();
        if self.inner.derived_is_dirty() {
            return self.source_summaries();
        }
        let files = self.issue_files()?;
        let mut files_by_number = BTreeMap::new();
        for path in &files {
            let Some(number) = number_from_filename(path) else {
                return self.source_summaries();
            };
            let file = path
                .file_name()
                .and_then(|name| name.to_str())
                .context(format!("source filename is not UTF-8: {}", path.display()))?
                .to_owned();
            if files_by_number.insert(number, file).is_some() {
                return self.source_summaries();
            }
        }

        let mut summaries = self.inner.summaries()?;
        if summaries.len() != files_by_number.len() {
            return self.source_summaries();
        }
        for summary in &mut summaries {
            let Some(file) = files_by_number.remove(&summary.number) else {
                return self.source_summaries();
            };
            summary.file = file;
        }
        Ok(summaries)
    }

    fn source_summaries(&self) -> Result<Vec<IssueSummary>> {
        Ok(self
            .scan_sources_lenient()?
            .into_iter()
            .map(IssueSource::summary)
            .collect())
    }

    fn repair_derived_best_effort(&self) {
        if !self.inner.derived_is_dirty() {
            return;
        }
        let repair = self.lock().map(|lock| {
            self.repair_derived_best_effort_locked(&lock);
        });
        if let Err(error) = repair {
            ErrorLog::record(&format!(
                "issue derived rebuild remains scheduled after read: {error:#}"
            ));
        }
    }

    fn repair_derived_best_effort_locked(&self, _lock: &StoreLock) {
        if !self.inner.derived_is_dirty() {
            return;
        }
        if let Err(error) = self.inner.repair_derived_locked() {
            ErrorLog::record(&format!(
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

    /// Return the zero or one source file that can safely represent `number`.
    ///
    /// Sorting before reporting an ambiguity makes the typed error independent
    /// of filesystem directory iteration order.
    fn unique_files_for(&self, number: u32) -> Result<Vec<PathBuf>> {
        let mut files = self.files_for(number)?;
        files.sort();
        if files.len() > 1 {
            return Err(AmbiguousIssueNumber { number, files }.into());
        }
        Ok(files)
    }

    fn unique_path_for(&self, number: u32) -> Result<Option<PathBuf>> {
        Ok(self.unique_files_for(number)?.into_iter().next())
    }

    fn source_dir_exists(&self) -> Result<bool> {
        self.dir()
            .try_exists()
            .with_context(|| format!("failed to inspect {}", self.dir().display()))
    }

    fn read_numbered_path(&self, number: u32, path: &Path) -> Result<Issue> {
        let issue = self.inner.read_existing_path(path)?;
        if issue.number != number {
            return Err(MismatchedIssueNumber {
                filename_number: number,
                declared_number: issue.number,
                file: path.to_path_buf(),
            }
            .into());
        }
        Ok(issue)
    }

    fn ensure_existing_number_matches(&self, number: u32, files: &[PathBuf]) -> Result<()> {
        for path in files {
            if let Ok(issue) = self.inner.read_existing_path(path)
                && issue.number != number
            {
                return Err(MismatchedIssueNumber {
                    filename_number: number,
                    declared_number: issue.number,
                    file: path.clone(),
                }
                .into());
            }
        }
        Ok(())
    }

    fn allocation_dir(&self) -> Result<PathBuf> {
        let dot_git = self.repo_root.join(".git");
        if dot_git.is_dir() {
            return Ok(dot_git.join(ALLOCATION_DIR_NAME));
        }
        if !dot_git.exists() {
            // Unit tests and repositories not yet initialized by git still
            // serialize local writers. A shared git directory is unavailable
            // in this case, so no cross-worktree guarantee is possible.
            return Ok(self.dir().join(ALLOCATION_DIR_NAME));
        }

        let git_dir = git_dir_from_dot_git(&dot_git)?;
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
                return Err(error).context(format!(
                    "failed to read git common directory {}",
                    common_dir_file.display()
                ));
            }
        };
        Ok(common_dir.join(ALLOCATION_DIR_NAME))
    }
}

/// Resolve a worktree's `.git` file to its private git directory.
fn git_dir_from_dot_git(dot_git: &Path) -> Result<PathBuf> {
    let text = fs::read_to_string(dot_git).context(format!(
        "failed to read git directory file {}",
        dot_git.display()
    ))?;
    let path = text
        .strip_prefix("gitdir: ")
        .or_else(|| text.strip_prefix("gitdir:\t"))
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .context(format!("invalid git directory file {}", dot_git.display()))?;
    let path = Path::new(path);
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        dot_git.with_file_name(path)
    })
}

/// Whether `path` is an issue markdown file.
fn is_issue_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("md")
}

/// The issue number encoded in an issue file's name (`NNN-slug.md`), or `None`
/// when the name has no numeric prefix.
pub(crate) fn number_from_filename(path: &Path) -> Option<u32> {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.split_once('-'))
        .and_then(|(number, _)| number.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::issue::{IssuePriority, IssueStatus};
    use crate::infrastructure::persistence::json_file::{AtomicWriteStage, fail_next_atomic_write};
    use crate::infrastructure::store::DerivedState;
    use chrono::{TimeZone, Utc};
    use std::sync::{Arc, Barrier};
    use std::thread;

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

        assert!(
            tmp.path()
                .join(".usagi/issues/001-first-issue.md")
                .is_file()
        );
        assert!(store.index_path().is_file());
        assert_eq!(store.read(1).unwrap().unwrap(), i);
        assert_eq!(store.max_number().unwrap(), 1);
    }

    #[test]
    fn duplicate_number_read_write_and_remove_fail_closed_without_changing_source() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(7, "First")).unwrap();
        let first = store.dir().join("007-first.md");
        let second = store.dir().join("007-second.md");
        fs::write(&second, issue(7, "Second").to_markdown()).unwrap();
        let source_before = [fs::read(&first).unwrap(), fs::read(&second).unwrap()];
        let index_before = fs::read(store.index_path()).unwrap();
        let dirty = store.dir().join(".derived-dirty");
        fs::write(&dirty, b"pre-existing rebuild request\n").unwrap();
        let dirty_before = fs::read(&dirty).unwrap();

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

        assert_eq!(fs::read(&first).unwrap(), source_before[0]);
        assert_eq!(fs::read(&second).unwrap(), source_before[1]);
        assert_eq!(fs::read(store.index_path()).unwrap(), index_before);
        assert_eq!(fs::read(dirty).unwrap(), dirty_before);
        assert!(!store.dir().join("007-replacement.md").exists());
    }

    #[test]
    fn read_locks_before_ambiguity_check_and_leaves_scheduled_repair_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(7, "First")).unwrap();
        let first = store.dir().join("007-first.md");
        let second = store.dir().join("007-second.md");
        let dirty = store.dir().join(".derived-dirty");
        fs::write(&dirty, b"pre-existing rebuild request\n").unwrap();
        let index_before = fs::read(store.index_path()).unwrap();
        let dirty_before = fs::read(&dirty).unwrap();

        let held = store.lock().unwrap();
        let started = Arc::new(Barrier::new(2));
        let reader = {
            let root = tmp.path().to_path_buf();
            let started = Arc::clone(&started);
            thread::spawn(move || {
                started.wait();
                IssueStore::new(root).read(7).unwrap_err()
            })
        };
        started.wait();
        fs::write(&second, issue(7, "Second").to_markdown()).unwrap();
        drop(held);

        let error = reader.join().unwrap();
        let ambiguity = error.downcast_ref::<AmbiguousIssueNumber>().unwrap();
        assert_eq!(ambiguity.files, vec![first, second]);
        assert_eq!(fs::read(store.index_path()).unwrap(), index_before);
        assert_eq!(fs::read(dirty).unwrap(), dirty_before);
    }

    #[test]
    fn parseable_filename_frontmatter_mismatch_fails_closed_for_point_operations() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(7, "First")).unwrap();
        let path = store.dir().join("007-first.md");
        fs::write(&path, issue(8, "First").to_markdown()).unwrap();
        let source_before = fs::read(&path).unwrap();

        for error in [
            store.read(7).unwrap_err(),
            store.write(&issue(7, "Replacement")).unwrap_err(),
            store.remove(7).unwrap_err(),
        ] {
            let mismatch = error.downcast_ref::<MismatchedIssueNumber>().unwrap();
            assert_eq!(mismatch.filename_number, 7);
            assert_eq!(mismatch.declared_number, 8);
            assert_eq!(mismatch.file, path);
        }
        assert_eq!(fs::read(path).unwrap(), source_before);
        assert!(!store.dir().join("007-replacement.md").exists());
    }

    #[test]
    fn summaries_retain_exact_files_when_filename_identity_is_noncanonical() {
        let no_prefix_tmp = tempfile::tempdir().unwrap();
        let no_prefix_store = IssueStore::new(no_prefix_tmp.path());
        no_prefix_store.write(&issue(1, "One")).unwrap();
        fs::rename(
            no_prefix_store.dir().join("001-one.md"),
            no_prefix_store.dir().join("manual.md"),
        )
        .unwrap();
        assert_eq!(no_prefix_store.summaries().unwrap()[0].file, "manual.md");

        let mismatch_tmp = tempfile::tempdir().unwrap();
        let mismatch_store = IssueStore::new(mismatch_tmp.path());
        mismatch_store.write(&issue(1, "One")).unwrap();
        fs::rename(
            mismatch_store.dir().join("001-one.md"),
            mismatch_store.dir().join("002-moved.md"),
        )
        .unwrap();
        assert_eq!(mismatch_store.summaries().unwrap()[0].file, "002-moved.md");
    }

    #[test]
    fn index_records_the_format_version_and_summaries() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "First")).unwrap();

        let text = fs::read_to_string(store.index_path()).unwrap();
        assert!(text.contains("\"version\": 2"));
        assert!(text.contains("\"source_fingerprint\": \"sha256:"));
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
        unsafe {
            std::env::set_var(crate::infrastructure::paths::DATA_DIR_ENV, home.path());
        }

        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();
        fs::write(store.index_path(), "{ not json").unwrap();

        let summaries = store.summaries().unwrap();
        assert_eq!(summaries.len(), 1);
        let text = fs::read_to_string(store.index_path()).unwrap();
        assert!(text.contains("\"version\": 2"));

        let entry = fs::read_dir(
            crate::infrastructure::paths::data_dir()
                .unwrap()
                .join("logs"),
        )
        .expect("logs dir exists")
        .next()
        .expect("a log file was written")
        .expect("readable entry");
        assert!(
            fs::read_to_string(entry.path())
                .unwrap()
                .contains("is corrupt")
        );

        unsafe {
            std::env::remove_var(crate::infrastructure::paths::DATA_DIR_ENV);
        }
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
        unsafe {
            std::env::set_var(crate::infrastructure::paths::DATA_DIR_ENV, home.path());
        }

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

        let entry = fs::read_dir(
            crate::infrastructure::paths::data_dir()
                .unwrap()
                .join("logs"),
        )
        .expect("logs dir exists")
        .next()
        .expect("a log file was written")
        .expect("readable entry");
        assert!(
            fs::read_to_string(entry.path())
                .unwrap()
                .contains("skipping unparseable issue file")
        );

        unsafe {
            std::env::remove_var(crate::infrastructure::paths::DATA_DIR_ENV);
        }
    }

    #[test]
    fn scan_errors_when_the_issues_path_is_not_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".usagi")).unwrap();
        fs::write(tmp.path().join(".usagi/issues"), "not a dir").unwrap();
        let store = IssueStore::new(tmp.path());

        assert!(
            store
                .scan()
                .unwrap_err()
                .to_string()
                .contains("failed to read")
        );
    }

    #[test]
    fn summaries_error_when_the_index_is_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();
        fs::remove_file(store.index_path()).unwrap();
        fs::create_dir(store.index_path()).unwrap();

        assert!(
            store
                .summaries()
                .unwrap_err()
                .to_string()
                .contains("failed to read")
        );
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

        assert!(
            store
                .read(3)
                .unwrap_err()
                .to_string()
                .contains("failed to read")
        );
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
    fn summaries_rebuild_after_same_count_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();
        let mut cached = issue(1, "One").summary();
        cached.title = "Stale cache".to_string();
        store.write_index(&[cached]).unwrap();

        fs::rename(
            store.dir().join("001-one.md"),
            store.dir().join("001-renamed.md"),
        )
        .unwrap();

        assert_eq!(store.summaries().unwrap()[0].title, "One");
    }

    #[test]
    fn summaries_rebuild_after_same_count_delete_and_add() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();
        fs::remove_file(store.dir().join("001-one.md")).unwrap();
        fs::write(
            store.dir().join("002-two.md"),
            issue(2, "Two").to_markdown(),
        )
        .unwrap();

        assert_eq!(store.summaries().unwrap()[0].number, 2);
    }

    #[test]
    fn summaries_rebuild_after_same_size_edit_with_preserved_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();
        let path = store.dir().join("001-one.md");
        set_mtime(&path, 1_000);
        let original_len = fs::metadata(&path).unwrap().len();

        fs::write(&path, issue(1, "Two").to_markdown()).unwrap();
        set_mtime(&path, 1_000);
        set_mtime(&store.index_path(), 1_000);

        assert_eq!(fs::metadata(&path).unwrap().len(), original_len);
        assert_eq!(store.summaries().unwrap()[0].title, "Two");
    }

    #[test]
    fn summaries_rebuild_after_edit_with_preserved_older_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();
        let path = store.dir().join("001-one.md");

        fs::write(&path, issue(1, "Updated title").to_markdown()).unwrap();
        set_mtime(&path, 1_000);
        set_mtime(&store.index_path(), 2_000);

        assert_eq!(store.summaries().unwrap()[0].title, "Updated title");
    }

    #[test]
    fn summaries_rebuild_legacy_and_unknown_fingerprint_metadata() {
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::infrastructure::paths::DATA_DIR_ENV, home.path());
        }
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();

        for metadata in [
            r#"{"version":1,"issues":[]}"#,
            r#"{"version":2,"source_fingerprint":"md5:unknown","issues":[]}"#,
        ] {
            fs::write(store.index_path(), metadata).unwrap();
            assert_eq!(store.summaries().unwrap()[0].title, "One");
        }
        unsafe {
            std::env::remove_var(crate::infrastructure::paths::DATA_DIR_ENV);
        }
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
    fn scan_lenient_returns_the_parseable_issues() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();
        assert_eq!(store.scan_lenient().unwrap().len(), 1);
    }

    #[test]
    fn reserve_next_number_reports_invalid_or_unreadable_sequence() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        assert_eq!(store.reserve_next_number().unwrap(), 1);

        let sequence = store
            .dir()
            .join(ALLOCATION_DIR_NAME)
            .join(ALLOCATION_FILE_NAME);
        fs::write(&sequence, "not a number\n").unwrap();
        assert!(store.reserve_next_number().is_err());

        fs::remove_file(&sequence).unwrap();
        fs::create_dir(&sequence).unwrap();
        assert!(store.reserve_next_number().is_err());
    }

    #[test]
    fn derived_index_failures_commit_create_and_self_heal_on_reopen() {
        let _guard = crate::test_support::process_env_guard();
        let logs = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::infrastructure::paths::DATA_DIR_ENV, logs.path());
        }

        for stage in [AtomicWriteStage::Write, AtomicWriteStage::Rename] {
            let tmp = tempfile::tempdir().unwrap();
            let store = IssueStore::new(tmp.path());
            let created = issue(1, "Created");
            fail_next_atomic_write(&store.index_path(), stage);

            let outcome = store.write(&created).unwrap();
            assert_eq!(outcome.derived, DerivedState::RebuildNeeded);
            assert_eq!(store.scan().unwrap(), vec![created.clone()]);

            let reopened = IssueStore::new(tmp.path());
            assert_eq!(reopened.read(1).unwrap(), Some(created.clone()));
            assert!(reopened.index_path().is_file());
            let retry = reopened.write(&created).unwrap();
            assert_eq!(retry.derived, DerivedState::Fresh);
            assert_eq!(reopened.scan().unwrap(), vec![created]);
        }

        unsafe {
            std::env::remove_var(crate::infrastructure::paths::DATA_DIR_ENV);
        }
    }

    #[test]
    fn derived_index_failures_commit_update_and_retry_same_identity() {
        let _guard = crate::test_support::process_env_guard();
        let logs = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::infrastructure::paths::DATA_DIR_ENV, logs.path());
        }

        for stage in [AtomicWriteStage::Write, AtomicWriteStage::Rename] {
            let tmp = tempfile::tempdir().unwrap();
            let store = IssueStore::new(tmp.path());
            store.write(&issue(1, "Old")).unwrap();
            let updated = issue(1, "Updated");
            fail_next_atomic_write(&store.index_path(), stage);

            let outcome = store.write(&updated).unwrap();
            assert_eq!(outcome.derived, DerivedState::RebuildNeeded);
            assert_eq!(store.scan().unwrap(), vec![updated.clone()]);

            let reopened = IssueStore::new(tmp.path());
            assert_eq!(reopened.read(1).unwrap(), Some(updated.clone()));
            assert_eq!(
                reopened.write(&updated).unwrap().derived,
                DerivedState::Fresh
            );
            assert_eq!(reopened.scan().unwrap(), vec![updated]);
        }

        unsafe {
            std::env::remove_var(crate::infrastructure::paths::DATA_DIR_ENV);
        }
    }

    #[test]
    fn derived_index_failures_commit_remove_and_retry_without_double_delete() {
        let _guard = crate::test_support::process_env_guard();
        let logs = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::infrastructure::paths::DATA_DIR_ENV, logs.path());
        }

        for stage in [AtomicWriteStage::Write, AtomicWriteStage::Rename] {
            let tmp = tempfile::tempdir().unwrap();
            let store = IssueStore::new(tmp.path());
            store.write(&issue(1, "Doomed")).unwrap();
            fail_next_atomic_write(&store.index_path(), stage);

            let outcome = store.remove_with_outcome(1).unwrap();
            assert!(outcome.value);
            assert_eq!(outcome.derived, DerivedState::RebuildNeeded);
            assert!(store.scan().unwrap().is_empty());

            let reopened = IssueStore::new(tmp.path());
            assert!(reopened.summaries().unwrap().is_empty());
            let retry = reopened.remove_with_outcome(1).unwrap();
            assert!(!retry.value);
            assert_eq!(retry.derived, DerivedState::Fresh);
        }

        unsafe {
            std::env::remove_var(crate::infrastructure::paths::DATA_DIR_ENV);
        }
    }

    #[test]
    fn source_atomic_failure_returns_error_without_mutating_issue() {
        for stage in [AtomicWriteStage::Write, AtomicWriteStage::Rename] {
            let tmp = tempfile::tempdir().unwrap();
            let store = IssueStore::new(tmp.path());
            let source = store.dir().join("001-created.md");
            fail_next_atomic_write(&source, stage);
            assert!(store.write(&issue(1, "Created")).is_err());
            assert!(store.scan().unwrap().is_empty());

            store.write(&issue(1, "Old")).unwrap();
            let updated_source = store.dir().join("001-updated.md");
            fail_next_atomic_write(&updated_source, stage);
            assert!(store.write(&issue(1, "Updated")).is_err());
            assert_eq!(store.read(1).unwrap().unwrap().title, "Old");
            assert!(!updated_source.exists());
        }
    }

    #[test]
    fn source_remove_failure_returns_error_without_removing_the_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        fs::create_dir_all(store.dir().join("001-weird.md")).unwrap();

        assert!(store.remove(1).is_err());
        assert!(store.dir().join("001-weird.md").is_dir());
    }

    #[test]
    fn next_mutation_rebuilds_all_sources_when_derived_was_already_dirty() {
        let _guard = crate::test_support::process_env_guard();
        let logs = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::infrastructure::paths::DATA_DIR_ENV, logs.path());
        }
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();
        fail_next_atomic_write(&store.index_path(), AtomicWriteStage::Rename);
        assert_eq!(
            store.write(&issue(2, "Two")).unwrap().derived,
            DerivedState::RebuildNeeded
        );

        assert_eq!(
            store.write(&issue(3, "Three")).unwrap().derived,
            DerivedState::Fresh
        );
        let numbers: Vec<_> = store
            .summaries()
            .unwrap()
            .into_iter()
            .map(|summary| summary.number)
            .collect();
        assert_eq!(numbers, vec![1, 2, 3]);

        unsafe {
            std::env::remove_var(crate::infrastructure::paths::DATA_DIR_ENV);
        }
    }

    #[test]
    fn dirty_remove_rebuilds_and_failed_read_repair_returns_source_summaries() {
        let _guard = crate::test_support::process_env_guard();
        let logs = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::infrastructure::paths::DATA_DIR_ENV, logs.path());
        }
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();
        fail_next_atomic_write(&store.index_path(), AtomicWriteStage::Rename);
        store.write(&issue(2, "Two")).unwrap();

        assert_eq!(
            store.remove_with_outcome(1).unwrap().derived,
            DerivedState::Fresh
        );
        fail_next_atomic_write(&store.index_path(), AtomicWriteStage::Rename);
        store.write(&issue(3, "Three")).unwrap();
        fail_next_atomic_write(&store.index_path(), AtomicWriteStage::Rename);
        let numbers: Vec<_> = store
            .summaries()
            .unwrap()
            .into_iter()
            .map(|summary| summary.number)
            .collect();
        assert_eq!(numbers, vec![2, 3]);

        unsafe {
            std::env::remove_var(crate::infrastructure::paths::DATA_DIR_ENV);
        }
    }

    #[test]
    fn allocation_directory_resolves_git_and_worktree_layouts() {
        let repository = tempfile::tempdir().unwrap();
        let store = IssueStore::new(repository.path());
        fs::create_dir(repository.path().join(".git")).unwrap();
        assert_eq!(
            store.allocation_dir().unwrap(),
            repository.path().join(".git").join(ALLOCATION_DIR_NAME)
        );

        fs::remove_dir(repository.path().join(".git")).unwrap();
        let private_git = repository.path().join("private-git");
        fs::create_dir(&private_git).unwrap();
        fs::write(repository.path().join(".git"), "gitdir: private-git\n").unwrap();
        assert_eq!(
            store.allocation_dir().unwrap(),
            private_git.join(ALLOCATION_DIR_NAME)
        );

        let common = repository.path().join("common-git");
        fs::create_dir(&common).unwrap();
        fs::write(
            private_git.join("commondir"),
            common.to_string_lossy().as_bytes(),
        )
        .unwrap();
        assert_eq!(
            store.allocation_dir().unwrap(),
            common.join(ALLOCATION_DIR_NAME)
        );

        fs::write(private_git.join("commondir"), "../relative-common\n").unwrap();
        assert_eq!(
            store.allocation_dir().unwrap(),
            private_git
                .join("../relative-common")
                .join(ALLOCATION_DIR_NAME)
        );
    }

    #[test]
    fn malformed_git_indirection_and_common_directory_io_are_errors() {
        let repository = tempfile::tempdir().unwrap();
        let store = IssueStore::new(repository.path());
        fs::write(repository.path().join(".git"), "not a gitdir\n").unwrap();
        assert!(
            store
                .allocation_dir()
                .unwrap_err()
                .to_string()
                .contains("invalid git directory file")
        );
        fs::write(repository.path().join(".git"), "gitdir: \n").unwrap();
        assert!(git_dir_from_dot_git(&repository.path().join(".git")).is_err());

        let private_git = repository.path().join("private-git");
        fs::create_dir(&private_git).unwrap();
        fs::write(
            repository.path().join(".git"),
            format!("gitdir:\t{}\n", private_git.display()),
        )
        .unwrap();
        fs::create_dir(private_git.join("commondir")).unwrap();
        assert!(
            store
                .allocation_dir()
                .unwrap_err()
                .to_string()
                .contains("failed to read git common directory")
        );
        assert!(
            git_dir_from_dot_git(&repository.path().join("missing-dot-git"))
                .unwrap_err()
                .to_string()
                .contains("failed to read git directory file")
        );
    }

    #[test]
    fn derived_marker_clear_failure_keeps_rebuild_scheduled() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        fs::create_dir_all(store.dir().join(".derived-dirty")).unwrap();
        let outcome = store.inner.finish_committed((), Ok(()));
        assert_eq!(outcome.derived, DerivedState::RebuildNeeded);
        assert!(store.inner.derived_is_dirty());
    }
}

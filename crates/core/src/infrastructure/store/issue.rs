//! Persistence for a repository's task issues.
//!
//! Issues live as `NNN-<slug>.md` files under `<repo>/.usagi/issues/`, each a
//! frontmatter markdown document (see [`crate::domain::issue`]). The markdown
//! files are the source of truth; `index.json` alongside them is a derived
//! cache of the metadata that speeds up listings and is rebuilt from the files
//! whenever it is missing, unreadable, or stale relative to the markdown files.
//! Markdown files are meant to be committed and shared; `index.json` is a local
//! rebuildable cache, so it is never relied upon for correctness — only speed.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::domain::frontmatter::FrontmatterDoc;
use crate::domain::issue::{Issue, IssueSummary};
use crate::infrastructure::error_log::ErrorLog;
use crate::infrastructure::paths::{SESSIONS_DIR, STATE_DIR};
use crate::infrastructure::persistence::markdown_store::{MarkdownEntry, MarkdownStore};
use crate::infrastructure::persistence::store_lock::StoreLock;
use crate::infrastructure::store::MutationOutcome;
use crate::infrastructure::store::issue_number_sequence::{
    ExistingIssueFloors, IssueNumberSequence,
};

const ISSUES_DIR_NAME: &str = "issues";

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
    pub filename_number: Option<u32>,
    pub declared_number: u32,
    pub file: PathBuf,
}

impl fmt::Display for MismatchedIssueNumber {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.filename_number {
            Some(filename_number) => write!(
                formatter,
                "issue source {} declares #{} but its filename claims #{}; refusing point operation",
                self.file.display(),
                self.declared_number,
                filename_number
            ),
            None => write!(
                formatter,
                "issue source {} declares #{} but its filename has no numeric issue claim; refusing point operation",
                self.file.display(),
                self.declared_number
            ),
        }
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

/// Checkpoints that must retain one store lock while a search snapshot is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceSnapshotLockPhase {
    LockAcquired,
    DerivedRepaired,
    SnapshotCaptured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadLockPhase {
    UniqueChecked,
    DerivedRepaired,
    ExactRead,
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
        self.source_snapshot_observing_lock(|_| {})
    }

    fn source_snapshot_observing_lock(
        &self,
        mut observe: impl FnMut(SourceSnapshotLockPhase),
    ) -> Result<IssueSourceSnapshot> {
        if !self.source_dir_exists()? {
            return Ok(IssueSourceSnapshot::default());
        }
        let lock = self.lock()?;
        observe(SourceSnapshotLockPhase::LockAcquired);
        self.repair_derived_best_effort_locked(&lock);
        observe(SourceSnapshotLockPhase::DerivedRepaired);
        let snapshot = self.source_snapshot_locked(&lock)?;
        observe(SourceSnapshotLockPhase::SnapshotCaptured);
        Ok(snapshot)
    }

    #[cfg(test)]
    pub(crate) fn source_snapshot_after_lock_for_test(
        &self,
        observe: impl FnMut(SourceSnapshotLockPhase),
    ) -> Result<IssueSourceSnapshot> {
        self.source_snapshot_observing_lock(observe)
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

    /// Highest identity claim in either a filename or parseable frontmatter.
    ///
    /// Allocation cannot rely on filename prefixes alone: a prefixless source,
    /// or `007-*.md` declaring `number: 800`, already fences the declared side
    /// from point CRUD and must also prevent #800 from being handed out again.
    /// Unlike lenient listing, allocation fails on an unreadable/unparseable
    /// source because its declared high-water cannot be proven.
    fn max_claimed_number(&self) -> Result<u32> {
        let mut maximum = 0;
        for path in self.issue_files()? {
            maximum = maximum.max(number_from_filename(&path).unwrap_or(0));
            maximum = maximum.max(self.inner.read_existing_path(&path)?.number);
        }
        Ok(maximum)
    }

    /// Reserve the next issue number across every worktree of this workspace.
    ///
    /// A worktree has its own checked-out `.usagi/issues` directory, so its
    /// local maximum alone cannot safely allocate a number while another
    /// worktree is creating an issue. The v1-compatible authority below Git's
    /// common directory serializes every reservation. Its high-water sequence,
    /// durable journal, retired v2 sequence, and every workspace source maximum
    /// are folded without ever reusing a gap.
    ///
    /// # Errors
    ///
    /// Returns an error when the allocation lock or sequence cannot be read or
    /// written.
    pub fn reserve_next_number(&self) -> Result<u32> {
        let initial_workspace_root = workspace_root(&self.repo_root);
        let sequence =
            IssueNumberSequence::new(&self.repo_root, &initial_workspace_root, self.dir())?;
        let worktree_root = sequence.worktree_root().to_path_buf();
        let workspace_root = workspace_root(&worktree_root);
        sequence.reserve_with_floors(|| {
            self.existing_issue_floors(
                &workspace_root,
                &worktree_root,
                &sequence.registered_worktrees()?,
                &sequence.materialized_git_issue_roots()?,
            )
        })
    }

    /// Read a single issue by number, or `None` if it does not exist.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory cannot be read or the backing file
    /// cannot be read or parsed, or when more than one source file claims
    /// `number`.
    pub fn read(&self, number: u32) -> Result<Option<Issue>> {
        self.read_observing_lock(number, |_| {})
    }

    fn read_observing_lock(
        &self,
        number: u32,
        mut observe: impl FnMut(ReadLockPhase),
    ) -> Result<Option<Issue>> {
        if !self.source_dir_exists()? {
            return Ok(None);
        }
        let lock = self.lock()?;
        let path = self.point_path_for(number)?;
        observe(ReadLockPhase::UniqueChecked);
        self.repair_derived_best_effort_locked(&lock);
        observe(ReadLockPhase::DerivedRepaired);
        let issue = path
            .map(|path| self.read_numbered_path(number, &path))
            .transpose()?;
        observe(ReadLockPhase::ExactRead);
        Ok(issue)
    }

    /// Read source while the caller already holds the store lock.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory or source file cannot be read, or
    /// when more than one source file claims `number`.
    pub fn read_locked(&self, number: u32) -> Result<Option<Issue>> {
        self.point_path_for(number)?
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
        let existing: Vec<_> = self.point_path_for(issue.number)?.into_iter().collect();
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
        let files: Vec<_> = self.point_path_for(number)?.into_iter().collect();
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
            self.inner.reindex_after_remove()
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

    /// Resolve the only source that can safely represent `number`.
    ///
    /// One directory enumeration supplies both filename claims and parseable
    /// frontmatter claims. A mismatch fences point operations from either side:
    /// `007-moved.md` declaring `number: 8` blocks both #7 and #8, including
    /// when a separate canonical `008-*.md` exists. Read/parse errors for the
    /// requested filename claim propagate before any source or derived mutation.
    fn point_path_for(&self, number: u32) -> Result<Option<PathBuf>> {
        let mut files = self.issue_files()?;
        files.sort();
        let filename_claims: Vec<_> = files
            .iter()
            .filter(|path| number_from_filename(path) == Some(number))
            .cloned()
            .collect();
        if filename_claims.len() > 1 {
            let mut all_claims = filename_claims;
            for path in &files {
                if number_from_filename(path) == Some(number) {
                    continue;
                }
                if self
                    .inner
                    .read_existing_path(path)
                    .is_ok_and(|issue| issue.number == number)
                {
                    all_claims.push(path.clone());
                }
            }
            all_claims.sort();
            all_claims.dedup();
            return Err(AmbiguousIssueNumber {
                number,
                files: all_claims,
            }
            .into());
        }

        let direct = filename_claims.into_iter().next();
        if let Some(path) = &direct {
            let issue = self.inner.read_existing_path(path)?;
            if issue.number != number {
                return Err(MismatchedIssueNumber {
                    filename_number: Some(number),
                    declared_number: issue.number,
                    file: path.clone(),
                }
                .into());
            }
        }

        for path in files {
            if direct.as_ref() == Some(&path) {
                continue;
            }
            // An unrelated corrupt source has no trustworthy declared claim and
            // therefore cannot be associated with this requested number. A
            // parseable source does participate from its declared-number side.
            match self.inner.read_existing_path(&path) {
                Ok(issue) if issue.number == number => {
                    return Err(MismatchedIssueNumber {
                        filename_number: number_from_filename(&path),
                        declared_number: issue.number,
                        file: path,
                    }
                    .into());
                }
                Ok(_) | Err(_) => {}
            }
        }
        Ok(direct)
    }

    fn source_dir_exists(&self) -> Result<bool> {
        self.inner.source_dir_exists()
    }

    fn read_numbered_path(&self, number: u32, path: &Path) -> Result<Issue> {
        let issue = self.inner.read_existing_path(path)?;
        if issue.number != number {
            return Err(MismatchedIssueNumber {
                filename_number: Some(number),
                declared_number: issue.number,
                file: path.to_path_buf(),
            }
            .into());
        }
        Ok(issue)
    }

    /// Highest filename claims seen by fixed v2.
    ///
    /// No source maximum is marked v1-visible: two old-v1 callers sharing the
    /// Git-common authority can derive different workspace roots (for example,
    /// an external linked worktree). Only their shared sequence and journal are
    /// universal. Fixed v2 also discovers tracked, untracked, and ignored
    /// arbitrary nested issue stores across every registered worktree. This
    /// deliberately does not acquire sibling store locks: doing so under the
    /// authority lock would introduce a cross-worktree deadlock.
    fn existing_issue_floors(
        &self,
        workspace_root: &Path,
        worktree_root: &Path,
        registered_worktrees: &[PathBuf],
        materialized_issue_roots: &[PathBuf],
    ) -> Result<ExistingIssueFloors> {
        let mut v1_roots = BTreeSet::from([workspace_root.to_path_buf()]);
        let sessions = workspace_root.join(STATE_DIR).join(SESSIONS_DIR);
        match fs::read_dir(&sessions) {
            Ok(entries) => {
                for entry in entries {
                    let entry = context_session_entry(entry, &sessions)?;
                    let entry_path = entry.path();
                    let file_type = context_session_file_type(&entry_path, entry.file_type())?;
                    anyhow::ensure!(
                        !file_type.is_symlink(),
                        "session entry is a symlink and cannot be safely enumerated: {}",
                        entry_path.display()
                    );
                    if file_type.is_dir() {
                        v1_roots.insert(entry_path);
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::symlink_metadata(&sessions) {
                    Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => {}
                    Ok(_) | Err(_) => {
                        return Err(error)
                            .context(format!("failed to read sessions {}", sessions.display()));
                    }
                }
            }
            Err(error) => {
                return Err(error)
                    .context(format!("failed to read sessions {}", sessions.display()));
            }
        }

        let mut all_roots = v1_roots.clone();
        all_roots.insert(worktree_root.to_path_buf());
        all_roots.insert(self.repo_root.clone());
        all_roots.extend(registered_worktrees.iter().cloned());
        all_roots.extend(materialized_issue_roots.iter().cloned());

        let mut all = 0;
        for root in all_roots {
            let maximum = Self::new(&root).max_claimed_number()?;
            all = all.max(maximum);
        }
        Ok(ExistingIssueFloors { all, v1_visible: 0 })
    }
}

fn context_session_entry(
    entry: std::io::Result<fs::DirEntry>,
    sessions: &Path,
) -> Result<fs::DirEntry> {
    entry.context(format!(
        "failed to read a session entry in {}",
        sessions.display()
    ))
}

fn context_session_file_type(
    path: &Path,
    file_type: std::io::Result<fs::FileType>,
) -> Result<fs::FileType> {
    file_type.context(format!(
        "failed to inspect session entry {}",
        path.display()
    ))
}

/// Resolve the workspace root from a conventional session worktree path.
fn workspace_root(start: &Path) -> PathBuf {
    let mut prefix = PathBuf::new();
    let mut components = start.components().peekable();
    while let Some(component) = components.next() {
        if matches!(component, Component::Normal(name) if name == STATE_DIR)
            && components.peek().is_some_and(
                |next| matches!(*next, Component::Normal(name) if name == SESSIONS_DIR),
            )
        {
            return prefix;
        }
        prefix.push(component.as_os_str());
    }
    start.to_path_buf()
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
    use fs2::FileExt;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use std::sync::Arc;
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

    fn assert_duplicate_diagnostic_includes_declared_claim(declared_file: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(8, "Canonical A")).unwrap();
        let canonical_a = store.dir().join("008-canonical-a.md");
        let canonical_b = store.dir().join("008-canonical-b.md");
        let declared = store.dir().join(declared_file);
        fs::write(&canonical_b, issue(8, "Canonical B").to_markdown()).unwrap();
        fs::write(&declared, issue(8, "Declared").to_markdown()).unwrap();
        let dirty = store.dir().join(".derived-dirty");
        fs::write(&dirty, b"pre-existing rebuild request\n").unwrap();
        let mut expected = vec![canonical_a.clone(), canonical_b.clone(), declared.clone()];
        expected.sort();
        let source_before: Vec<_> = expected
            .iter()
            .map(|path| fs::read(path).unwrap())
            .collect();
        let index_before = fs::read(store.index_path()).unwrap();
        let dirty_before = fs::read(&dirty).unwrap();

        for error in [
            store.read(8).unwrap_err(),
            store.write(&issue(8, "Replacement")).unwrap_err(),
            store.remove(8).unwrap_err(),
        ] {
            let ambiguity = error.downcast_ref::<AmbiguousIssueNumber>().unwrap();
            assert_eq!(ambiguity.number, 8);
            assert_eq!(ambiguity.files, expected);
        }
        for (path, bytes) in expected.iter().zip(source_before) {
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
        assert_eq!(fs::read(store.index_path()).unwrap(), index_before);
        assert_eq!(fs::read(dirty).unwrap(), dirty_before);
        assert!(!store.dir().join("008-replacement.md").exists());
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
    fn duplicate_diagnostics_include_mismatched_and_prefixless_declared_claims() {
        assert_duplicate_diagnostic_includes_declared_claim("007-moved.md");
        assert_duplicate_diagnostic_includes_declared_claim("manual.md");
    }

    #[test]
    fn read_holds_one_lock_across_unique_check_repair_and_exact_parse() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(7, "First")).unwrap();
        let dirty = store.dir().join(".derived-dirty");
        fs::write(&dirty, b"pre-existing rebuild request\n").unwrap();
        let lock_path = Arc::new(StoreLock::path(store.dir()));
        let mut observed = Vec::new();

        let read = store
            .read_observing_lock(7, |phase| {
                observed.push(phase);
                let lock_path = Arc::clone(&lock_path);
                let competing_writer_acquired = thread::spawn(move || {
                    let file = fs::File::options()
                        .read(true)
                        .write(true)
                        .open(lock_path.as_ref())
                        .unwrap();
                    file.try_lock_exclusive().is_ok()
                })
                .join()
                .unwrap();
                assert!(
                    !competing_writer_acquired,
                    "cooperative writer acquired the store lock during {phase:?}"
                );
            })
            .unwrap()
            .unwrap();

        assert_eq!(read, issue(7, "First"));
        assert_eq!(
            observed,
            [
                ReadLockPhase::UniqueChecked,
                ReadLockPhase::DerivedRepaired,
                ReadLockPhase::ExactRead,
            ]
        );
        assert!(!dirty.exists());
        let competing_writer = StoreLock::acquire(store.dir()).unwrap();
        drop(competing_writer);
    }

    #[test]
    fn summaries_fall_back_to_sources_when_scheduled_repair_cannot_lock() {
        let _guard = crate::test_support::process_env_guard();
        let logs = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::infrastructure::paths::DATA_DIR_ENV, logs.path());
        }
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        fs::create_dir_all(store.dir()).unwrap();
        let dirty = store.dir().join(".derived-dirty");
        fs::write(&dirty, b"pre-existing rebuild request\n").unwrap();
        fs::create_dir(store.dir().join(".lock")).unwrap();

        assert!(store.summaries().unwrap().is_empty());
        assert!(dirty.exists());

        unsafe {
            std::env::remove_var(crate::infrastructure::paths::DATA_DIR_ENV);
        }
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
            assert_eq!(mismatch.filename_number, Some(7));
            assert_eq!(mismatch.declared_number, 8);
            assert_eq!(mismatch.file, path);
        }
        assert_eq!(fs::read(path).unwrap(), source_before);
        assert!(!store.dir().join("007-replacement.md").exists());
    }

    #[test]
    fn numbered_path_parser_rejects_a_mismatched_declaration() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        fs::create_dir_all(store.dir()).unwrap();
        let path = store.dir().join("007-moved.md");
        fs::write(&path, issue(8, "Moved").to_markdown()).unwrap();

        let error = store.read_numbered_path(7, &path).unwrap_err();
        let mismatch = error.downcast_ref::<MismatchedIssueNumber>().unwrap();
        assert_eq!(mismatch.filename_number, Some(7));
        assert_eq!(mismatch.declared_number, 8);
        assert_eq!(mismatch.file, path);
    }

    #[test]
    fn unparseable_filename_claim_blocks_write_and_remove_without_any_effect() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(7, "Original")).unwrap();
        let source = store.dir().join("007-original.md");
        fs::write(&source, b"not valid issue markdown\n").unwrap();
        let dirty = store.dir().join(".derived-dirty");
        fs::write(&dirty, b"pre-existing rebuild request\n").unwrap();
        let source_before = fs::read(&source).unwrap();
        let index_before = fs::read(store.index_path()).unwrap();
        let dirty_before = fs::read(&dirty).unwrap();

        for error in [
            store.write(&issue(7, "Replacement")).unwrap_err(),
            store.remove(7).unwrap_err(),
        ] {
            assert!(error.to_string().contains("failed to parse"));
            assert!(error.to_string().contains(source.to_str().unwrap()));
            assert!(
                error
                    .root_cause()
                    .downcast_ref::<crate::domain::issue::ParseIssueError>()
                    .is_some()
            );
        }

        assert_eq!(fs::read(&source).unwrap(), source_before);
        assert_eq!(fs::read(store.index_path()).unwrap(), index_before);
        assert_eq!(fs::read(&dirty).unwrap(), dirty_before);
        assert!(!store.dir().join("007-replacement.md").exists());
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_filename_claim_blocks_write_and_remove_without_any_effect() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(7, "Original")).unwrap();
        let source = store.dir().join("007-original.md");
        let source_before = fs::read(&source).unwrap();
        let original_permissions = fs::metadata(&source).unwrap().permissions();
        let dirty = store.dir().join(".derived-dirty");
        fs::write(&dirty, b"pre-existing rebuild request\n").unwrap();
        let index_before = fs::read(store.index_path()).unwrap();
        let dirty_before = fs::read(&dirty).unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o000)).unwrap();

        for error in [
            store.write(&issue(7, "Replacement")).unwrap_err(),
            store.remove(7).unwrap_err(),
        ] {
            assert!(error.to_string().contains("failed to read"));
            assert!(error.to_string().contains(source.to_str().unwrap()));
            assert_eq!(
                error
                    .root_cause()
                    .downcast_ref::<std::io::Error>()
                    .unwrap()
                    .kind(),
                std::io::ErrorKind::PermissionDenied
            );
        }

        fs::set_permissions(&source, original_permissions).unwrap();
        assert_eq!(fs::read(&source).unwrap(), source_before);
        assert_eq!(fs::read(store.index_path()).unwrap(), index_before);
        assert_eq!(fs::read(&dirty).unwrap(), dirty_before);
        assert!(!store.dir().join("007-replacement.md").exists());
    }

    #[test]
    fn filename_and_declared_claims_fence_both_numbers_without_any_effect() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(7, "Moved")).unwrap();
        store.write(&issue(8, "Canonical")).unwrap();
        let moved = store.dir().join("007-moved.md");
        let canonical = store.dir().join("008-canonical.md");
        fs::write(&moved, issue(8, "Moved").to_markdown()).unwrap();
        let dirty = store.dir().join(".derived-dirty");
        fs::write(&dirty, b"pre-existing rebuild request\n").unwrap();
        let sources_before = [fs::read(&moved).unwrap(), fs::read(&canonical).unwrap()];
        let index_before = fs::read(store.index_path()).unwrap();
        let dirty_before = fs::read(&dirty).unwrap();

        for error in [
            store.read(7).unwrap_err(),
            store.write(&issue(7, "Replacement seven")).unwrap_err(),
            store.remove(7).unwrap_err(),
            store.read(8).unwrap_err(),
            store.write(&issue(8, "Replacement eight")).unwrap_err(),
            store.remove(8).unwrap_err(),
        ] {
            let mismatch = error.downcast_ref::<MismatchedIssueNumber>().unwrap();
            assert_eq!(mismatch.filename_number, Some(7));
            assert_eq!(mismatch.declared_number, 8);
            assert_eq!(mismatch.file, moved);
        }

        assert_eq!(fs::read(&moved).unwrap(), sources_before[0]);
        assert_eq!(fs::read(&canonical).unwrap(), sources_before[1]);
        assert_eq!(fs::read(store.index_path()).unwrap(), index_before);
        assert_eq!(fs::read(&dirty).unwrap(), dirty_before);
        assert!(!store.dir().join("007-replacement-seven.md").exists());
        assert!(!store.dir().join("008-replacement-eight.md").exists());
    }

    #[test]
    fn prefixless_declared_claim_fences_its_number_without_any_effect() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(8, "Moved")).unwrap();
        let manual = store.dir().join("manual.md");
        fs::rename(store.dir().join("008-moved.md"), &manual).unwrap();
        let dirty = store.dir().join(".derived-dirty");
        fs::write(&dirty, b"pre-existing rebuild request\n").unwrap();
        let source_before = fs::read(&manual).unwrap();
        let index_before = fs::read(store.index_path()).unwrap();
        let dirty_before = fs::read(&dirty).unwrap();

        for error in [
            store.read(8).unwrap_err(),
            store.write(&issue(8, "Replacement")).unwrap_err(),
            store.remove(8).unwrap_err(),
        ] {
            assert!(
                error
                    .to_string()
                    .contains("filename has no numeric issue claim")
            );
            let mismatch = error.downcast_ref::<MismatchedIssueNumber>().unwrap();
            assert_eq!(mismatch.filename_number, None);
            assert_eq!(mismatch.declared_number, 8);
            assert_eq!(mismatch.file, manual);
        }

        assert_eq!(fs::read(&manual).unwrap(), source_before);
        assert_eq!(fs::read(store.index_path()).unwrap(), index_before);
        assert_eq!(fs::read(&dirty).unwrap(), dirty_before);
        assert!(!store.dir().join("008-replacement.md").exists());
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
        assert_eq!(
            store
                .issue_files()
                .unwrap()
                .iter()
                .filter(|path| number_from_filename(path) == Some(1))
                .count(),
            1
        );
        assert_eq!(store.read(1).unwrap().unwrap().title, "New title");
    }

    #[test]
    fn point_read_matches_any_numeric_prefix_not_just_zero_padded() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "First")).unwrap();

        let dir = store.dir().to_path_buf();
        fs::rename(dir.join("001-first.md"), dir.join("1-first.md")).unwrap();

        assert_eq!(
            store
                .issue_files()
                .unwrap()
                .iter()
                .filter(|path| number_from_filename(path) == Some(1))
                .count(),
            1
        );
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
        assert_eq!(
            store
                .issue_files()
                .unwrap()
                .iter()
                .filter(|path| number_from_filename(path) == Some(1))
                .count(),
            1
        );
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
    fn reserve_next_number_folds_every_workspace_source_maximum() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join(".usagi/sessions");
        let current_root = sessions.join("current");
        let sibling_root = sessions.join("sibling");
        let current = IssueStore::new(&current_root);
        let sibling = IssueStore::new(&sibling_root);
        fs::create_dir_all(sibling.dir()).unwrap();
        fs::write(
            sibling.dir().join("515-unmerged.md"),
            issue(515, "Unmerged").to_markdown(),
        )
        .unwrap();
        for root in [tmp.path(), current_root.as_path(), sibling_root.as_path()] {
            let legacy = root.join(".usagi/issues/usagi-issue-sequence").join("next");
            fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            fs::write(&legacy, "migrated-to-usagi-issue-numbers:515\n").unwrap();
        }

        assert_eq!(current.reserve_next_number().unwrap(), 516);
        assert!(
            tmp.path()
                .join(".usagi/issue-numbers/reservations/0000000516.reserved")
                .is_file()
        );
    }

    #[test]
    fn reserve_next_number_folds_filename_and_declared_source_claims() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git(root, &["init", "-q"]);

        let store = IssueStore::new(root);
        fs::create_dir_all(store.dir()).unwrap();
        let mismatched = store.dir().join("007-declares-800.md");
        let prefixless = store.dir().join("declared-only.md");
        fs::write(
            &mismatched,
            issue(800, "Declared eight hundred").to_markdown(),
        )
        .unwrap();
        fs::write(
            &prefixless,
            issue(900, "Declared nine hundred").to_markdown(),
        )
        .unwrap();
        let mismatched_before = fs::read(&mismatched).unwrap();
        let prefixless_before = fs::read(&prefixless).unwrap();

        let authority = root.join(".git/usagi/issue-numbers");
        fs::create_dir_all(&authority).unwrap();
        fs::write(
            authority.join("sequence.json"),
            b"{\"version\":1,\"last_reserved\":500}\n",
        )
        .unwrap();
        fs::write(authority.join("legacy-v2-migrated"), b"500\n").unwrap();
        let legacy = root.join(".git/usagi-issue-sequence/next");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, b"migrated-to-usagi-issue-numbers:500\n").unwrap();

        assert_eq!(store.reserve_next_number().unwrap(), 901);
        assert_eq!(fs::read(&mismatched).unwrap(), mismatched_before);
        assert_eq!(fs::read(&prefixless).unwrap(), prefixless_before);
        assert_eq!(
            fs::read_to_string(authority.join("reservations/0000000901.reserved")).unwrap(),
            "901\n"
        );
        let sequence: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(authority.join("sequence.json")).unwrap())
                .unwrap();
        assert_eq!(sequence["last_reserved"], 901);
        assert_eq!(
            fs::read_to_string(&legacy).unwrap(),
            "migrated-to-usagi-issue-numbers:500\n"
        );
        assert_eq!(
            fs::read_to_string(authority.join("legacy-v2-migrated")).unwrap(),
            "500\n"
        );
    }

    #[test]
    fn reserve_next_number_rejects_an_unparseable_source_without_any_effect() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git(root, &["init", "-q"]);

        let store = IssueStore::new(root);
        fs::create_dir_all(store.dir()).unwrap();
        let source = store.dir().join("declared-floor-is-opaque.md");
        fs::write(&source, b"not issue frontmatter\n").unwrap();
        let index = store.index_path();
        fs::write(&index, b"pre-existing derived bytes\n").unwrap();
        let dirty = store.dir().join(".derived-dirty");
        fs::write(&dirty, b"pre-existing rebuild request\n").unwrap();

        let authority = root.join(".git/usagi/issue-numbers");
        let reservations = authority.join("reservations");
        fs::create_dir_all(&reservations).unwrap();
        let sequence = authority.join("sequence.json");
        let reservation = reservations.join("0000000500.reserved");
        let migration = authority.join("legacy-v2-migrated");
        fs::write(&sequence, b"{\"version\":1,\"last_reserved\":500}\n").unwrap();
        fs::write(&reservation, b"500\n").unwrap();
        fs::write(&migration, b"500\n").unwrap();
        let legacy = root.join(".git/usagi-issue-sequence/next");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, b"migrated-to-usagi-issue-numbers:500\n").unwrap();

        let preserved = [
            fs::read(&source).unwrap(),
            fs::read(&index).unwrap(),
            fs::read(&dirty).unwrap(),
            fs::read(&sequence).unwrap(),
            fs::read(&reservation).unwrap(),
            fs::read(&migration).unwrap(),
            fs::read(&legacy).unwrap(),
        ];
        assert!(store.reserve_next_number().is_err());
        assert_eq!(fs::read(&source).unwrap(), preserved[0]);
        assert_eq!(fs::read(&index).unwrap(), preserved[1]);
        assert_eq!(fs::read(&dirty).unwrap(), preserved[2]);
        assert_eq!(fs::read(&sequence).unwrap(), preserved[3]);
        assert_eq!(fs::read(&reservation).unwrap(), preserved[4]);
        assert_eq!(fs::read(&migration).unwrap(), preserved[5]);
        assert_eq!(fs::read(&legacy).unwrap(), preserved[6]);
        assert!(!reservations.join("0000000501.reserved").exists());
    }

    #[test]
    fn nested_main_and_linked_worktree_calls_share_authority_and_root_source_maximum() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "user.name", "Test"]);
        fs::write(root.join("README.md"), "workspace\n").unwrap();
        git(&root, &["add", "README.md"]);
        git(&root, &["commit", "-q", "-m", "init"]);

        let root_store = IssueStore::new(&root);
        root_store
            .write(&issue(515, "Existing root source"))
            .unwrap();
        let nested_main = root.join("crates/core");
        fs::create_dir_all(&nested_main).unwrap();

        let sessions = root.join(".usagi/sessions");
        fs::create_dir_all(&sessions).unwrap();
        let linked = sessions.join("linked");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "test-linked",
                linked.to_str().unwrap(),
            ],
        );
        let nested_linked = linked.join("crates/core");
        fs::create_dir_all(&nested_linked).unwrap();

        let authority = root.join(".git/usagi/issue-numbers");
        fs::create_dir_all(&authority).unwrap();
        fs::write(
            authority.join("sequence.json"),
            b"{\"version\":1,\"last_reserved\":515}\n",
        )
        .unwrap();
        fs::write(authority.join("legacy-v2-migrated"), b"515\n").unwrap();
        let common_legacy = root.join(".git/usagi-issue-sequence/next");
        let nested_main_legacy = nested_main.join(".usagi/issues/usagi-issue-sequence/next");
        for legacy in [&common_legacy, &nested_main_legacy] {
            fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            fs::write(legacy, b"migrated-to-usagi-issue-numbers:515\n").unwrap();
        }

        assert_eq!(
            IssueStore::new(&nested_main).reserve_next_number().unwrap(),
            516
        );
        assert_eq!(
            IssueStore::new(&nested_linked)
                .reserve_next_number()
                .unwrap(),
            517
        );
        assert!(authority.join("reservations/0000000516.reserved").is_file());
        assert!(authority.join("reservations/0000000517.reserved").is_file());
        assert!(!nested_main.join(".usagi/issue-numbers").exists());
        assert!(!nested_linked.join(".usagi/issue-numbers").exists());

        let external = tmp.path().join("external-linked");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "test-external-source-floor",
                external.to_str().unwrap(),
            ],
        );
        let external_store = IssueStore::new(&external);
        fs::create_dir_all(external_store.dir()).unwrap();
        fs::write(
            external_store.dir().join("800-manual.md"),
            issue(800, "External source floor").to_markdown(),
        )
        .unwrap();

        assert_eq!(
            IssueStore::new(&nested_main).reserve_next_number().unwrap(),
            801
        );
        assert!(authority.join("reservations/0000000801.reserved").is_file());
    }

    #[test]
    fn source_only_floor_is_not_assumed_visible_to_every_old_v1_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "user.name", "Test"]);
        fs::write(root.join("README.md"), "workspace\n").unwrap();
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
                "test-stale-v1-source-visibility",
                external.to_str().unwrap(),
            ],
        );
        let store = IssueStore::new(&root);
        let nested_store = IssueStore::new(root.join("tools/nested"));
        fs::create_dir_all(nested_store.dir()).unwrap();
        let source = nested_store.dir().join("800-nested-only.md");
        fs::write(&source, issue(800, "Nested-only floor").to_markdown()).unwrap();

        let authority = root.join(".git/usagi/issue-numbers");
        fs::create_dir_all(&authority).unwrap();
        let sequence = authority.join("sequence.json");
        fs::write(&sequence, r#"{"version":1,"last_reserved":500}"#).unwrap();
        let common_legacy = root.join(".git/usagi-issue-sequence/next");
        fs::create_dir_all(common_legacy.parent().unwrap()).unwrap();
        fs::write(&common_legacy, "migrated-to-usagi-issue-numbers:500\n").unwrap();
        fs::write(authority.join("legacy-v2-migrated"), "500\n").unwrap();
        let nested_legacy = nested_store.dir().join("usagi-issue-sequence/next");
        fs::create_dir_all(nested_legacy.parent().unwrap()).unwrap();
        fs::write(&nested_legacy, "600\n").unwrap();
        fs::write(root.join(".git/info/exclude"), "tools/nested/.usagi/\n").unwrap();
        let source_before = fs::read(&source).unwrap();
        let sequence_before = fs::read(&sequence).unwrap();
        let common_before = fs::read(&common_legacy).unwrap();
        let nested_before = fs::read(&nested_legacy).unwrap();

        let error = store.reserve_next_number().unwrap_err();
        assert!(error.to_string().contains("neither live legacy v2 nor v1"));
        assert_eq!(fs::read(&source).unwrap(), source_before);
        assert_eq!(fs::read(&sequence).unwrap(), sequence_before);
        assert_eq!(fs::read(&common_legacy).unwrap(), common_before);
        assert_eq!(fs::read(&nested_legacy).unwrap(), nested_before);
        assert!(!authority.join("reservations").exists());
        assert!(!external.join(".usagi").exists());

        fs::write(&nested_legacy, "800\n").unwrap();
        assert_eq!(store.reserve_next_number().unwrap(), 801);
        assert_eq!(fs::read(&source).unwrap(), source_before);
        assert!(authority.join("reservations/0000000801.reserved").is_file());
    }

    #[test]
    fn nested_source_floor_fences_its_missing_legacy_authority_in_production_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git(root, &["init", "-q"]);

        let nested_store = IssueStore::new(root.join("tools/nested"));
        fs::create_dir_all(nested_store.dir()).unwrap();
        let source = nested_store.dir().join("800-known.md");
        fs::write(&source, issue(800, "Known nested source").to_markdown()).unwrap();
        let source_before = fs::read(&source).unwrap();

        let authority = root.join(".git/usagi/issue-numbers");
        fs::create_dir_all(&authority).unwrap();
        fs::write(
            authority.join("sequence.json"),
            r#"{"version":1,"last_reserved":800}"#,
        )
        .unwrap();
        fs::write(authority.join("legacy-v2-migrated"), "800\n").unwrap();
        let shared_legacy = root.join(".git/usagi-issue-sequence/next");
        fs::create_dir_all(shared_legacy.parent().unwrap()).unwrap();
        fs::write(&shared_legacy, "migrated-to-usagi-issue-numbers:800\n").unwrap();
        let nested_legacy = nested_store.dir().join("usagi-issue-sequence/next");
        assert!(!nested_legacy.exists());

        assert_eq!(IssueStore::new(root).reserve_next_number().unwrap(), 801);
        assert_eq!(fs::read(&source).unwrap(), source_before);
        assert_eq!(
            fs::read_to_string(&nested_legacy).unwrap(),
            "migrated-to-usagi-issue-numbers:801\n"
        );
        assert!(authority.join("reservations/0000000801.reserved").is_file());
    }

    #[test]
    fn stale_gitfile_targets_fail_without_creating_split_authority_state() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".git"), "gitdir: missing-private\n").unwrap();
        let store = IssueStore::new(tmp.path());

        assert!(store.reserve_next_number().is_err());
        assert!(!tmp.path().join("missing-private").exists());
        assert!(!tmp.path().join(".usagi").exists());

        let tmp = tempfile::tempdir().unwrap();
        let private = tmp.path().join("private");
        fs::create_dir(&private).unwrap();
        fs::write(tmp.path().join(".git"), "gitdir: private\n").unwrap();
        fs::write(private.join("commondir"), "../missing-common\n").unwrap();
        let store = IssueStore::new(tmp.path());

        assert!(store.reserve_next_number().is_err());
        assert!(!tmp.path().join("missing-common").exists());
        assert!(!private.join("usagi").exists());
        assert!(!tmp.path().join(".usagi").exists());
    }

    #[test]
    fn unreadable_workspace_session_listing_fails_before_reservation() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".usagi")).unwrap();
        fs::write(tmp.path().join(".usagi/sessions"), "not a directory").unwrap();
        let store = IssueStore::new(tmp.path());

        assert!(store.reserve_next_number().is_err());
        assert!(
            !tmp.path()
                .join(".usagi/issue-numbers/reservations")
                .exists()
        );
    }

    #[test]
    fn session_entry_iteration_errors_keep_their_operation_context() {
        let sessions = Path::new("/workspace/.usagi/sessions");
        let entry_error = std::io::Error::other("entry vanished");
        let error = context_session_entry(Err(entry_error), sessions)
            .err()
            .unwrap();
        assert!(error.to_string().contains("failed to read a session entry"));
        assert_eq!(
            error
                .root_cause()
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .kind(),
            std::io::ErrorKind::Other
        );

        let entry_path = sessions.join("vanished");
        let type_error = std::io::Error::new(std::io::ErrorKind::NotFound, "entry vanished");
        let error = context_session_file_type(&entry_path, Err(type_error)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("failed to inspect session entry")
        );
        assert_eq!(
            error
                .root_cause()
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_session_entry_fails_source_floor_discovery_without_effect() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let real_session = root.join("real-session");
        let real_store = IssueStore::new(&real_session);
        fs::create_dir_all(real_store.dir()).unwrap();
        let source = real_store.dir().join("800-real.md");
        fs::write(&source, issue(800, "Real session source").to_markdown()).unwrap();
        let legacy = real_store.dir().join("usagi-issue-sequence/next");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, b"800\n").unwrap();
        let sessions = root.join(".usagi/sessions");
        fs::create_dir_all(&sessions).unwrap();
        let session_entry = sessions.join("linked");
        symlink(&real_session, &session_entry).unwrap();

        let authority = root.join(".usagi/issue-numbers");
        let sequence = authority.join("sequence.json");
        let reservation = authority.join("reservations/0000000500.reserved");
        fs::create_dir_all(reservation.parent().unwrap()).unwrap();
        fs::write(&sequence, b"{\"version\":1,\"last_reserved\":500}\n").unwrap();
        fs::write(&reservation, b"500\n").unwrap();
        let sequence_before = fs::read(&sequence).unwrap();
        let reservation_before = fs::read(&reservation).unwrap();
        let source_before = fs::read(&source).unwrap();
        let legacy_before = fs::read(&legacy).unwrap();

        let store = IssueStore::new(root);
        let error = store
            .existing_issue_floors(root, root, &[], &[])
            .unwrap_err();
        assert!(error.to_string().contains("session entry is a symlink"));
        assert_eq!(fs::read(&sequence).unwrap(), sequence_before);
        assert_eq!(fs::read(&reservation).unwrap(), reservation_before);
        assert_eq!(fs::read(&source).unwrap(), source_before);
        assert_eq!(fs::read(&legacy).unwrap(), legacy_before);
        assert!(
            fs::symlink_metadata(&session_entry)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!authority.join("legacy-v2-migrated").exists());
        assert!(!authority.join("reservations/0000000501.reserved").exists());
        assert!(!store.dir().exists());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_or_unreadable_sessions_fail_source_floor_discovery() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dangling_tmp = tempfile::tempdir().unwrap();
        let dangling_root = dangling_tmp.path();
        fs::create_dir(dangling_root.join(STATE_DIR)).unwrap();
        let dangling_sessions = dangling_root.join(STATE_DIR).join(SESSIONS_DIR);
        symlink("missing-sessions", &dangling_sessions).unwrap();
        let dangling_store = IssueStore::new(dangling_root);
        let error = dangling_store
            .existing_issue_floors(dangling_root, dangling_root, &[], &[])
            .unwrap_err();
        assert!(error.to_string().contains("failed to read sessions"));
        assert_eq!(
            fs::read_link(&dangling_sessions).unwrap(),
            PathBuf::from("missing-sessions")
        );

        let unreadable_tmp = tempfile::tempdir().unwrap();
        let unreadable_root = unreadable_tmp.path();
        let unreadable_sessions = unreadable_root.join(STATE_DIR).join(SESSIONS_DIR);
        fs::create_dir_all(&unreadable_sessions).unwrap();
        let original = fs::metadata(&unreadable_sessions).unwrap().permissions();
        fs::set_permissions(&unreadable_sessions, fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable_store = IssueStore::new(unreadable_root);
        let error = unreadable_store
            .existing_issue_floors(unreadable_root, unreadable_root, &[], &[])
            .unwrap_err();
        fs::set_permissions(&unreadable_sessions, original).unwrap();
        assert!(error.to_string().contains("failed to read sessions"));
        assert_eq!(
            error
                .root_cause()
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }

    fn assert_no_local_issue_reservation(root: &Path) {
        assert!(!root.join(".usagi/issue-numbers/sequence.json").exists());
        assert!(!root.join(".usagi/issue-numbers/reservations").exists());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_session_or_issue_store_fails_without_a_reservation() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".usagi")).unwrap();
        let sessions = tmp.path().join(".usagi/sessions");
        symlink("missing-sessions", &sessions).unwrap();
        let store = IssueStore::new(tmp.path());

        assert!(store.reserve_next_number().is_err());
        assert_eq!(
            fs::read_link(&sessions).unwrap(),
            PathBuf::from("missing-sessions")
        );
        assert_no_local_issue_reservation(tmp.path());
        assert!(!store.dir().exists());

        let tmp = tempfile::tempdir().unwrap();
        let session = tmp.path().join(".usagi/sessions/sibling");
        fs::create_dir_all(session.join(".usagi")).unwrap();
        let issues = session.join(".usagi/issues");
        symlink("missing-issues", &issues).unwrap();
        let store = IssueStore::new(tmp.path());

        assert!(store.reserve_next_number().is_err());
        assert_eq!(
            fs::read_link(&issues).unwrap(),
            PathBuf::from("missing-issues")
        );
        assert_no_local_issue_reservation(tmp.path());

        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".usagi")).unwrap();
        let issues = tmp.path().join(".usagi/issues");
        symlink("missing-issues", &issues).unwrap();
        let dangling_store = IssueStore::new(tmp.path());
        assert!(dangling_store.max_number().is_err());
        assert!(dangling_store.read(1).is_err());
        assert!(dangling_store.source_snapshot().is_err());
        assert_eq!(
            fs::read_link(&issues).unwrap(),
            PathBuf::from("missing-issues")
        );

        let tmp = tempfile::tempdir().unwrap();
        let session = tmp.path().join(".usagi/sessions/sibling");
        fs::create_dir_all(&session).unwrap();
        let state = session.join(".usagi");
        symlink("missing-state", &state).unwrap();
        let dangling_store = IssueStore::new(&session);
        assert!(dangling_store.max_number().is_err());
        assert!(dangling_store.read(1).is_err());
        assert!(dangling_store.source_snapshot().is_err());
        assert_eq!(
            fs::read_link(&state).unwrap(),
            PathBuf::from("missing-state")
        );

        let root_store = IssueStore::new(tmp.path());
        assert!(root_store.reserve_next_number().is_err());
        assert_eq!(
            fs::read_link(&state).unwrap(),
            PathBuf::from("missing-state")
        );
        assert_no_local_issue_reservation(tmp.path());

        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join(".usagi/sessions");
        let actual = tmp.path().join("actual-session");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir(&actual).unwrap();
        let linked = sessions.join("linked");
        symlink(&actual, &linked).unwrap();
        let store = IssueStore::new(tmp.path());
        assert!(store.reserve_next_number().is_err());
        assert_eq!(fs::read_link(&linked).unwrap(), actual);
        assert_no_local_issue_reservation(tmp.path());
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
    fn derived_marker_clear_failure_keeps_rebuild_scheduled() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        fs::create_dir_all(store.dir().join(".derived-dirty")).unwrap();
        let outcome = store.inner.finish_committed((), Ok(()));
        assert_eq!(outcome.derived, DerivedState::RebuildNeeded);
        assert!(store.inner.derived_is_dirty());
    }
}

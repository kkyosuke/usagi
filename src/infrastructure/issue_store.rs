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
use crate::infrastructure::error_log::ErrorLog;
use crate::infrastructure::json_file;
use crate::infrastructure::repo_paths::STATE_DIR;
use crate::infrastructure::store_lock::StoreLock;

const ISSUES_DIR_NAME: &str = "issues";
/// Filename of the derived metadata cache. Kept out of git by the rules in
/// [`crate::infrastructure::gitignore`], which a test there cross-checks against
/// this constant.
pub(crate) const INDEX_FILE: &str = "index.json";
const FILE_FORMAT_VERSION: u32 = 1;

/// On-disk shape of `index.json`, read back as owned data. The `version` key is
/// written (see [`IndexFileRef`]) but ignored on read, so it is not modelled
/// here — serde skips unknown keys.
#[derive(Debug, Deserialize)]
struct IndexFile {
    issues: Vec<IssueSummary>,
}

/// Borrowed view used only when *writing* `index.json`, so the rebuild does not
/// have to clone every summary just to hand it to the serialiser.
#[derive(Serialize)]
struct IndexFileRef<'a> {
    version: u32,
    issues: &'a [IssueSummary],
}

/// File-based persistence rooted at a repository's `.usagi/issues/` directory.
pub struct IssueStore {
    dir: PathBuf,
}

impl IssueStore {
    /// Open the issue store for the repository at `repo_root`.
    pub fn new(repo_root: impl AsRef<Path>) -> Self {
        Self {
            dir: repo_root.as_ref().join(STATE_DIR).join(ISSUES_DIR_NAME),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn index_path(&self) -> PathBuf {
        self.dir.join(INDEX_FILE)
    }

    /// Acquire this store's cross-process write lock, blocking until it is free.
    ///
    /// Hold the returned guard across a whole read-modify-write that must be
    /// atomic with respect to other processes — most importantly allocating the
    /// next issue number and writing it: the lock guarantees a concurrent
    /// `create` cannot read the same `max_number` and reuse the number. The
    /// [`write`](Self::write) / [`remove`](Self::remove) entry points take the
    /// lock themselves; pass the guard to [`write_locked`](Self::write_locked)
    /// when you already hold it to extend the critical section.
    pub fn lock(&self) -> Result<StoreLock> {
        StoreLock::acquire(&self.dir)
    }

    /// Read and parse every issue markdown file, sorted by number.
    pub fn scan(&self) -> Result<Vec<Issue>> {
        use rayon::prelude::*;

        let mut issues: Vec<Issue> = self
            .issue_files()?
            .into_par_iter()
            .map(|path| {
                let text = fs::read_to_string(&path)
                    .context(format!("failed to read {}", path.display()))?;
                Issue::from_markdown(&text)
                    .with_context(|| format!("failed to parse {}", path.display()))
            })
            .collect::<Result<Vec<_>>>()?;

        issues.sort_by_key(|i| i.number);
        Ok(issues)
    }

    /// Like [`scan`](Self::scan) but **tolerant**: a markdown file that fails to
    /// read or parse is recorded to the daily error log and skipped, rather than
    /// failing the whole scan. Directory-level read failures still propagate.
    ///
    /// Used by [`rebuild_index`](Self::rebuild_index) so one corrupt or
    /// half-written issue file cannot fail an unrelated [`write`](Self::write)
    /// (whose target file is already persisted by the time the index rebuilds) or
    /// break `issue list` — the index simply rebuilds from the files that parse,
    /// mirroring how [`load_index`](Self::load_index) self-heals a corrupt cache.
    /// The strict [`scan`](Self::scan) stays the choice where every issue must be
    /// readable (e.g. the dependency graph).
    fn scan_lenient(&self) -> Result<Vec<Issue>> {
        use rayon::prelude::*;

        let parsed: Vec<(PathBuf, Result<Issue>)> = self
            .issue_files()?
            .into_par_iter()
            .map(|path| {
                let issue = fs::read_to_string(&path)
                    .context(format!("failed to read {}", path.display()))
                    .and_then(|text| {
                        Issue::from_markdown(&text)
                            .with_context(|| format!("failed to parse {}", path.display()))
                    });
                (path, issue)
            })
            .collect();

        let mut issues = Vec::with_capacity(parsed.len());
        for (path, issue) in parsed {
            match issue {
                Ok(issue) => issues.push(issue),
                Err(e) => ErrorLog::record(&format!(
                    "skipping unparseable issue file {} while rebuilding the index: {e:#}",
                    path.display()
                )),
            }
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
    ///
    /// Derives the number from each file's name (`NNN-slug.md`) — the files are
    /// the source of truth — instead of the `index.json` cache. Numbering is
    /// correctness-critical: a new issue is assigned `max_number + 1`, and
    /// [`write`](Self::write) deletes any file already backing that number. The
    /// cache can lag behind the files whenever issues are added, removed, or
    /// restored outside usagi (e.g. via `git pull` or a branch switch), so
    /// trusting it here could hand out a number that an existing file already
    /// uses — silently destroying that file. The name carries the same number
    /// [`write`] and [`read`](Self::read) key the file by, so reading it is as
    /// authoritative as parsing the body while skipping the read-and-parse of
    /// every issue's full markdown that a content scan would cost.
    pub fn max_number(&self) -> Result<u32> {
        Ok(self
            .issue_files()?
            .iter()
            .filter_map(|path| number_from_filename(path))
            .max()
            .unwrap_or(0))
    }

    /// Read a single issue by number, or `None` if it does not exist.
    ///
    /// Parses only the file(s) backing `number` instead of scanning and parsing
    /// the whole directory.
    pub fn read(&self, number: u32) -> Result<Option<Issue>> {
        let Some(path) = self.files_for(number)?.into_iter().next() else {
            return Ok(None);
        };
        let text =
            fs::read_to_string(&path).context(format!("failed to read {}", path.display()))?;
        let issue = Issue::from_markdown(&text)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        Ok(Some(issue))
    }

    /// Write `issue` to disk and refresh the index, taking the store lock for the
    /// duration so concurrent writers serialise.
    ///
    /// If a file for the same number already exists under a different name
    /// (because the title — and therefore the slug — changed), the stale file is
    /// removed so each issue is backed by exactly one file.
    pub fn write(&self, issue: &Issue) -> Result<()> {
        let lock = self.lock()?;
        self.write_locked(&lock, issue)
    }

    /// Like [`write`](Self::write) but assumes the caller already holds this
    /// store's [`lock`](Self::lock). Use this to keep number allocation and the
    /// write inside one lock acquisition (see [`crate::usecase::issue::create`]).
    ///
    /// Write order is deliberate: the new target file is written **first**, then
    /// any stale-named sibling for the same number is removed. A crash between
    /// the two therefore leaves the new file present (at worst a transient
    /// duplicate, which the next rebuild/scan reconciles) rather than an issue
    /// with no backing file.
    pub fn write_locked(&self, _lock: &StoreLock, issue: &Issue) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .context(format!("failed to create {}", self.dir.display()))?;

        let target = self.dir.join(issue.file_name());
        json_file::write_text_atomic(&target, &issue.to_markdown())?;

        for stale in self.files_for(issue.number)? {
            if stale != target {
                fs::remove_file(&stale).context(format!("failed to remove {}", stale.display()))?;
            }
        }

        self.rebuild_index()?;
        Ok(())
    }

    /// Remove the issue with `number`, returning whether anything was deleted,
    /// then refresh the index. Takes the store lock for the duration.
    pub fn remove(&self, number: u32) -> Result<bool> {
        let _lock = self.lock()?;
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
        match serde_json::from_str(&text) {
            Ok(index) => Ok(Some(index)),
            // A present-but-unparseable cache is recoverable — the caller rebuilds
            // from the markdown files — but it still signals real data corruption.
            // Record it so the silent self-heal leaves a trace in the daily log.
            Err(e) => {
                ErrorLog::record(&format!(
                    "issue index {} is corrupt; rebuilding from markdown: {e}",
                    self.index_path().display()
                ));
                Ok(None)
            }
        }
    }

    /// Rebuild `index.json` from the markdown files and return the summaries.
    fn rebuild_index(&self) -> Result<Vec<IssueSummary>> {
        // Tolerant scan: one corrupt sibling file must not fail a write whose own
        // file already landed, nor break `issue list`. The skipped files are
        // logged (see [`scan_lenient`](Self::scan_lenient)).
        let summaries: Vec<IssueSummary> =
            self.scan_lenient()?.iter().map(Issue::summary).collect();
        if summaries.is_empty() && !self.dir.exists() {
            // Nothing stored and no directory yet: don't create files eagerly.
            return Ok(summaries);
        }
        fs::create_dir_all(&self.dir)
            .context(format!("failed to create {}", self.dir.display()))?;
        let index = IndexFileRef {
            version: FILE_FORMAT_VERSION,
            issues: &summaries,
        };
        // The canonical "pretty JSON + trailing newline, written atomically" path
        // lives in `json_file::write_atomic`; reuse it rather than re-implementing
        // the serialise-and-write here.
        json_file::write_atomic(&self.dir, &self.index_path(), &index)?;
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

/// The issue number encoded in an issue file's name (`NNN-slug.md`), or `None`
/// when the name has no numeric prefix. This is the number
/// [`IssueStore::write`] names the file by (and [`IssueStore::files_for`] keys
/// it by), so it is an authoritative, parse-free source for
/// [`IssueStore::max_number`].
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
        // Recording the corruption writes to `<data dir>/logs/`, so pin the data
        // directory to a temp home to keep the test hermetic.
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, home.path());

        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();
        fs::write(store.index_path(), "{ not json").unwrap();

        let summaries = store.summaries().unwrap();
        assert_eq!(summaries.len(), 1);
        // The corrupt cache was replaced with a valid one.
        let text = fs::read_to_string(store.index_path()).unwrap();
        assert!(text.contains("\"version\": 1"));

        // The recoverable corruption is still recorded in the daily log.
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
    fn write_tolerates_a_corrupt_sibling_and_indexes_the_parseable_files() {
        // A corrupt sibling file used to fail every later write: `write` persists
        // its own file, then `rebuild_index` scanned *all* markdown and choked on
        // the bad one, so the write returned an error (with the new file already on
        // disk and the index stale). The rebuild is now tolerant. Pin the data dir
        // so the skip's log line is hermetic.
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, home.path());

        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "First")).unwrap();
        // A corrupt, unparseable issue file lands beside the valid one.
        fs::write(store.dir().join("002-broken.md"), "not an issue").unwrap();

        // Writing another issue still succeeds — the unrelated corrupt sibling is
        // skipped during the index rebuild instead of failing the whole write.
        store.write(&issue(3, "Third")).unwrap();

        // The index rebuilt from the files that parse (1 and 3); the corrupt one
        // is skipped — but the strict `scan` still surfaces it for callers that
        // need every issue readable (e.g. the dependency graph).
        let nums: Vec<u32> = store
            .summaries()
            .unwrap()
            .iter()
            .map(|s| s.number)
            .collect();
        assert_eq!(nums, vec![1, 3]);
        assert!(store.scan().is_err());

        // The skip is recorded in the daily log rather than silently swallowed.
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
        // A directory where the issue file should be makes reading it fail.
        fs::create_dir(store.dir().join("003-broken.md")).unwrap();

        assert!(store
            .read(3)
            .unwrap_err()
            .to_string()
            .contains("failed to read"));
    }

    #[test]
    fn max_number_reflects_files_added_outside_usagi() {
        // A stale index must not make `max_number` undercount. Regression test:
        // a markdown file added straight to disk (e.g. pulled from git) was
        // invisible to `max_number`, so `create` reused its number and `write`
        // deleted the existing file.
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        // Seed the store so index.json exists listing only issue #1.
        store.write(&issue(1, "One")).unwrap();
        // Issue #2 appears on disk without going through the store, leaving the
        // index stale (it still lists only #1).
        fs::write(
            store.dir().join("002-two.md"),
            issue(2, "Two").to_markdown(),
        )
        .unwrap();

        // The number must come from the files, not the stale cache.
        assert_eq!(store.max_number().unwrap(), 2);
    }

    #[test]
    fn creating_the_next_issue_does_not_clobber_a_file_missing_from_the_index() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();
        // An existing issue #2 the stale index doesn't know about.
        fs::write(
            store.dir().join("002-two.md"),
            issue(2, "Two").to_markdown(),
        )
        .unwrap();

        // Emulate `create`: the next number is max + 1, then the issue is
        // written.
        let next = store.max_number().unwrap() + 1;
        store.write(&issue(next, "Three")).unwrap();

        // #2 survives and all three issues are present.
        assert_eq!(next, 3);
        assert!(store.dir().join("002-two.md").exists());
        assert_eq!(store.scan().unwrap().len(), 3);
    }

    #[test]
    fn write_renames_in_place_leaving_one_valid_file_and_a_fresh_index() {
        // After a slug change `write` writes the NEW file first, THEN removes the
        // stale sibling (so a crash between the two leaves the new file, not a
        // file-less issue). The end state is exactly one valid backing file and
        // an index that reflects it.
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "Old title")).unwrap();

        let mut renamed = issue(1, "New title");
        renamed.body = "changed".to_string();
        store.write(&renamed).unwrap();

        // Exactly one file backs the issue and the stale name is gone.
        assert!(!store.dir().join("001-old-title.md").exists());
        assert!(store.dir().join("001-new-title.md").is_file());
        assert_eq!(store.files_for(1).unwrap().len(), 1);
        // The surviving file parses back to the new content.
        assert_eq!(store.read(1).unwrap().unwrap().title, "New title");
        // The index reflects the new title (the rebuild ran inside the lock).
        let index = fs::read_to_string(store.index_path()).unwrap();
        assert!(index.contains("\"title\": \"New title\""));
        assert!(!index.contains("Old title"));
    }

    #[test]
    fn the_lock_file_is_not_picked_up_as_an_issue() {
        let tmp = tempfile::tempdir().unwrap();
        let store = IssueStore::new(tmp.path());
        store.write(&issue(1, "One")).unwrap();
        // Acquiring the lock creates the `.lock` file inside the store dir; it
        // must never be parsed as an issue or counted by scans.
        let _guard = store.lock().unwrap();
        assert!(store.dir().join(".lock").is_file());
        assert_eq!(store.scan().unwrap().len(), 1);
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

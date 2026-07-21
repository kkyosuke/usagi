//! Persistence for a repository's agent memories.
//!
//! Memories live as `<name>.md` files under `<repo>/.usagi/memory/`, each a
//! frontmatter markdown document (see [`crate::domain::memory`]). The markdown
//! files are the source of truth. Two derived artifacts sit alongside them:
//!
//! - `MEMORY.md` — a human/agent-facing table of contents, committed and shared
//!   like the memory files themselves.
//! - `index.json` — a local rebuildable metadata cache used only for speed.
//!
//! The derived files are rebuilt whenever the cache is missing, unreadable, or
//! stale relative to the markdown source files.

use std::fmt::Write as _;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::domain::frontmatter::FrontmatterDoc;
use crate::domain::memory::{Memory, MemorySummary};
use crate::infrastructure::paths::STATE_DIR;
use crate::infrastructure::persistence::markdown_store::{MarkdownEntry, MarkdownStore};
use crate::infrastructure::persistence::store_lock::StoreLock;
use crate::infrastructure::store::MutationOutcome;

const MEMORY_DIR_NAME: &str = "memory";
const TOC_FILE: &str = "MEMORY.md";

#[derive(Debug, Deserialize)]
struct IndexFile {
    memories: Vec<MemorySummary>,
}

#[derive(Serialize)]
struct IndexFileRef<'a> {
    version: u32,
    memories: &'a [MemorySummary],
}

struct MemoryEntry;

impl MarkdownEntry for MemoryEntry {
    type Entry = Memory;
    type Summary = MemorySummary;
    type Key = String;
    type IndexFile = IndexFile;
    type IndexFileRef<'a> = IndexFileRef<'a>;

    const NAME: &'static str = "memory";

    fn is_entry_file(path: &Path) -> bool {
        is_memory_file(path)
    }

    fn parse_markdown(text: &str) -> Result<Memory> {
        Ok(Memory::from_markdown(text)?)
    }

    fn to_markdown(entry: &Memory) -> String {
        entry.to_markdown()
    }

    fn file_name(entry: &Memory) -> Result<String> {
        memory_file_name(&entry.name)
    }

    fn key(entry: &Memory) -> String {
        entry.name.clone()
    }

    fn key_from_summary(summary: &MemorySummary) -> String {
        summary.name.clone()
    }

    fn key_from_path(path: &Path) -> Option<String> {
        path.file_stem()
            .and_then(|stem| stem.to_str())
            .map(ToOwned::to_owned)
    }

    fn summary(entry: &Memory) -> MemorySummary {
        entry.summary()
    }

    fn sort_entries(entries: &mut Vec<Memory>) {
        entries.sort_by(|a, b| a.name.cmp(&b.name));
    }

    fn summaries_from_index(index: IndexFile) -> Vec<MemorySummary> {
        index.memories
    }

    fn index_file_ref(summaries: &[MemorySummary]) -> IndexFileRef<'_> {
        IndexFileRef {
            version: crate::infrastructure::persistence::json_file::FILE_FORMAT_VERSION,
            memories: summaries,
        }
    }

    fn write_extra_derived(dir: &Path, summaries: &[MemorySummary]) -> Result<()> {
        crate::infrastructure::persistence::json_file::write_text_atomic(
            &dir.join(TOC_FILE),
            &render_toc(summaries),
        )
    }
}

/// The on-disk filename for a memory `name`, rejecting anything that is not a
/// single safe path component so a name can never escape the memory directory.
fn memory_file_name(name: &str) -> Result<String> {
    let mut components = Path::new(name).components();
    let single_component =
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
    if !single_component {
        anyhow::bail!(
            "refusing to use {name:?} as a memory name: it must be a single path \
             component with no separators or `..`"
        );
    }
    Ok(format!("{name}.md"))
}

/// File-based persistence rooted at a repository's `.usagi/memory/` directory.
pub struct MemoryStore {
    inner: MarkdownStore<MemoryEntry>,
}

impl MemoryStore {
    /// Open the memory store for the repository at `repo_root`.
    #[must_use]
    pub fn new(repo_root: impl AsRef<Path>) -> Self {
        Self {
            inner: MarkdownStore::new(repo_root.as_ref().join(STATE_DIR).join(MEMORY_DIR_NAME)),
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

    #[must_use]
    pub fn toc_path(&self) -> PathBuf {
        self.dir().join(TOC_FILE)
    }

    /// Acquire this store's cross-process write lock, blocking until it is free.
    /// Hold the guard across read-modify-write operations that must be atomic.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock cannot be acquired.
    pub fn lock(&self) -> Result<StoreLock> {
        StoreLock::acquire(self.dir())
    }

    /// Read and parse every memory markdown file, sorted by name.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory cannot be read or any file fails to
    /// parse.
    pub fn scan(&self) -> Result<Vec<Memory>> {
        self.inner.scan()
    }

    /// Like [`scan`](Self::scan), but logs unreadable/unparseable memory files and
    /// skips them so one corrupt sibling cannot break listings or cache rebuilds.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory itself cannot be read.
    pub fn scan_lenient(&self) -> Result<Vec<Memory>> {
        self.inner.scan_lenient()
    }

    /// Read a single memory by name, or `None` if it does not exist.
    ///
    /// # Errors
    ///
    /// Returns an error when `name` is not a single safe path component, or the
    /// backing file exists but cannot be read or parsed.
    pub fn read(&self, name: &str) -> Result<Option<Memory>> {
        self.repair_derived_best_effort();
        self.read_locked(name)
    }

    /// Read source while the caller already holds the store lock.
    ///
    /// # Errors
    ///
    /// Returns an error when the name or source file is invalid.
    pub fn read_locked(&self, name: &str) -> Result<Option<Memory>> {
        let path = self.dir().join(memory_file_name(name)?);
        match self.inner.read_existing_path(&path) {
            Ok(memory) => Ok(Some(memory)),
            Err(e) if path_missing(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Write `memory` to disk and refresh the derived files, taking the store
    /// lock for the duration so concurrent writers serialise.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock cannot be acquired, the name is unsafe, or
    /// source cannot be committed. A derived refresh failure is in the outcome.
    pub fn write(&self, memory: &Memory) -> Result<MutationOutcome<()>> {
        let lock = self.lock()?;
        self.write_locked(&lock, memory)
    }

    /// Like [`write`](Self::write) but assumes the caller already holds this
    /// store's [`lock`](Self::lock).
    ///
    /// # Errors
    ///
    /// Returns an error when the name is unsafe, source cannot be committed, or
    /// the dirty marker cannot be scheduled.
    pub fn write_locked(&self, _lock: &StoreLock, memory: &Memory) -> Result<MutationOutcome<()>> {
        let rebuild_required = self.inner.derived_is_dirty();
        self.inner.mark_derived_dirty()?;
        self.inner.write_markdown(memory)?;
        let refresh = if rebuild_required {
            self.inner.rebuild_derived().map(|_| ())
        } else {
            self.inner.reindex_after_write(memory)
        };
        Ok(self.inner.finish_committed((), refresh))
    }

    /// Remove the memory with `name`, returning whether anything was deleted,
    /// then refresh the derived files. Takes the store lock for the duration.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock cannot be acquired, the name is unsafe, or
    /// source cannot be removed. Derived failure does not change the delete.
    pub fn remove(&self, name: &str) -> Result<bool> {
        Ok(self.remove_with_outcome(name)?.value)
    }

    /// Remove a memory and report whether committed source left derived files
    /// fresh or scheduled for rebuild.
    ///
    /// # Errors
    ///
    /// Returns an error only before the source removal commits.
    pub fn remove_with_outcome(&self, name: &str) -> Result<MutationOutcome<bool>> {
        let _lock = self.lock()?;
        let path = self.dir().join(memory_file_name(name)?);
        match fs::metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let repair = self.inner.repair_derived_locked();
                return Ok(self.inner.finish_committed(false, repair));
            }
            Err(error) => {
                return Err(error).context(format!("failed to inspect {}", path.display()));
            }
        }
        let rebuild_required = self.inner.derived_is_dirty();
        self.inner.mark_derived_dirty()?;
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) => return Err(e).context(format!("failed to remove {}", path.display())),
        }
        let refresh = if rebuild_required {
            self.inner.rebuild_derived().map(|_| ())
        } else {
            self.inner.reindex_after_remove(&name.to_string())
        };
        Ok(self.inner.finish_committed(true, refresh))
    }

    /// Metadata summaries for every memory.
    ///
    /// # Errors
    ///
    /// Returns an error when the index cannot be read and the markdown source
    /// cannot be rescanned.
    pub fn summaries(&self) -> Result<Vec<MemorySummary>> {
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
                "memory derived rebuild remains scheduled after read: {error:#}"
            ));
        }
    }
}

fn path_missing(error: &anyhow::Error) -> bool {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<std::io::Error>())
        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

/// Render the `MEMORY.md` table of contents: a heading and one bullet per memory
/// (newest first), each linking to its file with the type as a one-word hook.
fn render_toc(summaries: &[MemorySummary]) -> String {
    let mut sorted: Vec<&MemorySummary> = summaries.iter().collect();
    sorted.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.name.cmp(&b.name)));

    let mut out = String::from("# Memory\n\n");
    if sorted.is_empty() {
        out.push_str("_No memories yet._\n");
        return out;
    }
    for s in sorted {
        // Writing into a `String` is infallible, so the result is discarded.
        let _ = writeln!(out, "- [{}]({}) — {}", s.title, s.file, s.kind.as_str());
    }
    out
}

/// Whether `path` is a memory markdown file (a `*.md` that is not the table of
/// contents).
fn is_memory_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("md")
        && path.file_name().and_then(|n| n.to_str()) != Some(TOC_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::memory::MemoryType;
    use crate::infrastructure::persistence::json_file::{AtomicWriteStage, fail_next_atomic_write};
    use crate::infrastructure::store::DerivedState;
    use chrono::{TimeZone, Utc};

    fn memory(name: &str, title: &str) -> Memory {
        let ts = Utc.with_ymd_and_hms(2026, 6, 17, 0, 0, 0).unwrap();
        Memory {
            name: name.to_string(),
            title: title.to_string(),
            kind: MemoryType::Project,
            related: vec![],
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
        let store = MemoryStore::new(tmp.path());
        assert!(store.scan().unwrap().is_empty());
    }

    #[test]
    fn dir_and_paths_point_under_usagi_memory() {
        let store = MemoryStore::new("/repo");
        assert_eq!(store.dir(), Path::new("/repo/.usagi/memory"));
        assert_eq!(
            store.index_path(),
            PathBuf::from("/repo/.usagi/memory/index.json")
        );
        assert_eq!(
            store.toc_path(),
            PathBuf::from("/repo/.usagi/memory/MEMORY.md")
        );
    }

    #[test]
    fn write_then_read_round_trips_and_writes_derived_files() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path());
        let m = memory("first-fact", "First fact");

        store.write(&m).unwrap();

        assert!(tmp.path().join(".usagi/memory/first-fact.md").is_file());
        assert!(store.index_path().is_file());
        assert!(store.toc_path().is_file());
        assert_eq!(store.read("first-fact").unwrap().unwrap(), m);
    }

    #[test]
    fn read_returns_none_for_a_missing_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path());
        assert!(store.read("nope").unwrap().is_none());
    }

    #[test]
    fn read_and_remove_reject_a_path_traversing_name() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path());
        for bad in ["../../etc/passwd", "a/b", "..", "/etc/hosts"] {
            assert!(
                store.read(bad).is_err(),
                "read should reject the traversing name {bad:?}"
            );
            assert!(
                store.remove(bad).is_err(),
                "remove should reject the traversing name {bad:?}"
            );
            assert!(
                store.write(&memory(bad, "evil")).is_err(),
                "write should reject the traversing name {bad:?}"
            );
        }
        assert!(store.read("ok-name").unwrap().is_none());
    }

    #[test]
    fn index_records_version_and_toc_lists_the_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path());
        store.write(&memory("a-fact", "A fact")).unwrap();

        let index = fs::read_to_string(store.index_path()).unwrap();
        assert!(index.contains("\"version\": 1"));
        assert!(index.contains("\"name\": \"a-fact\""));

        let toc = fs::read_to_string(store.toc_path()).unwrap();
        assert!(toc.starts_with("# Memory\n"));
        assert!(toc.contains("- [A fact](a-fact.md) — project\n"));
    }

    #[test]
    fn write_same_name_overwrites_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path());
        store.write(&memory("fact", "Old")).unwrap();

        let mut updated = memory("fact", "New");
        updated.body = "changed".to_string();
        store.write(&updated).unwrap();

        assert_eq!(store.scan().unwrap().len(), 1);
        assert_eq!(store.read("fact").unwrap().unwrap().title, "New");
    }

    #[test]
    fn toc_orders_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path());
        let mut older = memory("older", "Older");
        older.updated_at = Utc.with_ymd_and_hms(2026, 6, 16, 0, 0, 0).unwrap();
        let mut newer = memory("newer", "Newer");
        newer.updated_at = Utc.with_ymd_and_hms(2026, 6, 18, 0, 0, 0).unwrap();
        store.write(&older).unwrap();
        store.write(&newer).unwrap();

        let toc = fs::read_to_string(store.toc_path()).unwrap();
        let newer_at = toc.find("Newer").unwrap();
        let older_at = toc.find("Older").unwrap();
        assert!(newer_at < older_at);
    }

    #[test]
    fn remove_deletes_the_file_and_reports_success() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path());
        store.write(&memory("doomed", "Doomed")).unwrap();

        assert!(store.remove("doomed").unwrap());
        assert!(store.read("doomed").unwrap().is_none());
        assert!(!store.remove("doomed").unwrap());
    }

    #[test]
    fn remove_rebuilds_the_derived_files_when_the_cache_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path());
        store.write(&memory("one", "One")).unwrap();
        store.write(&memory("two", "Two")).unwrap();
        fs::remove_file(store.index_path()).unwrap();

        assert!(store.remove("one").unwrap());

        let names: Vec<String> = store
            .summaries()
            .unwrap()
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert_eq!(names, vec!["two".to_string()]);
        assert!(store.index_path().is_file());
    }

    #[test]
    fn remove_leaves_the_cache_untouched_when_the_name_is_absent_from_it() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path());
        store.write(&memory("two", "Two")).unwrap();
        fs::write(
            store.dir().join("one.md"),
            memory("one", "One").to_markdown(),
        )
        .unwrap();

        assert!(store.remove("one").unwrap());
        let names: Vec<String> = store
            .summaries()
            .unwrap()
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert_eq!(names, vec!["two".to_string()]);
    }

    #[test]
    fn summaries_rebuild_when_index_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path());
        store.write(&memory("one", "One")).unwrap();
        store.write(&memory("two", "Two")).unwrap();

        fs::remove_file(store.index_path()).unwrap();
        let summaries = store.summaries().unwrap();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].name, "one");
        assert_eq!(summaries[1].name, "two");
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
        let store = MemoryStore::new(tmp.path());
        store.write(&memory("one", "One")).unwrap();
        fs::write(store.index_path(), "{ not json").unwrap();

        let summaries = store.summaries().unwrap();
        assert_eq!(summaries.len(), 1);
        let text = fs::read_to_string(store.index_path()).unwrap();
        assert!(text.contains("\"version\": 1"));

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
        let store = MemoryStore::new(tmp.path());
        assert!(store.summaries().unwrap().is_empty());
        assert!(!store.dir().exists());
    }

    #[test]
    fn scan_propagates_parse_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path());
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(store.dir().join("broken.md"), "not a memory").unwrap();

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
        let store = MemoryStore::new(tmp.path());
        store.write(&memory("one", "One")).unwrap();
        fs::write(store.dir().join("broken.md"), "not a memory").unwrap();

        store.write(&memory("two", "Two")).unwrap();
        let names: Vec<String> = store
            .summaries()
            .unwrap()
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert_eq!(names, vec!["one".to_string(), "two".to_string()]);

        fs::remove_file(store.index_path()).unwrap();
        let rebuilt: Vec<String> = store
            .summaries()
            .unwrap()
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert_eq!(rebuilt, vec!["one".to_string(), "two".to_string()]);
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
                .contains("skipping unparseable memory file")
        );

        unsafe {
            std::env::remove_var(crate::infrastructure::paths::DATA_DIR_ENV);
        }
    }

    #[test]
    fn scan_errors_when_the_memory_path_is_not_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".usagi")).unwrap();
        fs::write(tmp.path().join(".usagi/memory"), "not a dir").unwrap();
        let store = MemoryStore::new(tmp.path());

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
        let store = MemoryStore::new(tmp.path());
        store.write(&memory("one", "One")).unwrap();
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
        let store = MemoryStore::new(tmp.path());
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(store.dir().join("broken.md"), "not a memory").unwrap();

        let err = store.read("broken").unwrap_err();
        assert!(err.to_string().contains("failed to parse"));
    }

    #[test]
    fn read_errors_when_the_backing_file_is_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path());
        fs::create_dir_all(store.dir()).unwrap();
        fs::create_dir(store.dir().join("broken.md")).unwrap();

        assert!(
            store
                .read("broken")
                .unwrap_err()
                .to_string()
                .contains("failed to read")
        );
    }

    #[test]
    fn remove_errors_when_the_path_is_not_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path());
        fs::create_dir_all(store.dir()).unwrap();
        fs::create_dir(store.dir().join("weird.md")).unwrap();

        assert!(
            store
                .remove("weird")
                .unwrap_err()
                .to_string()
                .contains("failed to remove")
        );
    }

    #[test]
    fn the_lock_file_is_not_picked_up_as_a_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path());
        store.write(&memory("one", "One")).unwrap();
        let _guard = store.lock().unwrap();
        assert!(store.dir().join(".lock").is_file());
        assert_eq!(store.scan().unwrap().len(), 1);
    }

    #[test]
    fn toc_and_index_are_ignored_by_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path());
        store.write(&memory("one", "One")).unwrap();
        fs::write(store.dir().join("README.txt"), "ignore me").unwrap();

        assert_eq!(store.scan().unwrap().len(), 1);
    }

    #[test]
    fn memory_entry_key_from_path_uses_the_file_stem() {
        assert_eq!(
            MemoryEntry::key_from_path(Path::new("/repo/.usagi/memory/one.md")),
            Some("one".to_string())
        );
    }

    #[test]
    fn scan_lenient_returns_the_parseable_memories() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path());
        store.write(&memory("one", "One")).unwrap();
        assert_eq!(store.scan_lenient().unwrap().len(), 1);
    }

    #[test]
    fn summaries_rebuild_when_a_memory_file_is_newer_than_the_index() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path());
        store.write(&memory("one", "One")).unwrap();

        let edited = memory("one", "Updated title");
        let path = store.dir().join("one.md");
        fs::write(&path, edited.to_markdown()).unwrap();

        set_mtime(&store.index_path(), 1_000);
        set_mtime(&path, 2_000);

        assert_eq!(store.summaries().unwrap()[0].title, "Updated title");
    }

    #[test]
    fn derived_toc_failures_commit_create_and_self_heal_on_reopen() {
        let _guard = crate::test_support::process_env_guard();
        let logs = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::infrastructure::paths::DATA_DIR_ENV, logs.path());
        }

        for stage in [AtomicWriteStage::Write, AtomicWriteStage::Rename] {
            let tmp = tempfile::tempdir().unwrap();
            let store = MemoryStore::new(tmp.path());
            let created = memory("created", "Created");
            fail_next_atomic_write(&store.toc_path(), stage);

            let outcome = store.write(&created).unwrap();
            assert_eq!(outcome.derived, DerivedState::RebuildNeeded);
            assert_eq!(store.scan().unwrap(), vec![created.clone()]);

            let reopened = MemoryStore::new(tmp.path());
            assert_eq!(reopened.read("created").unwrap(), Some(created.clone()));
            assert!(reopened.toc_path().is_file());
            assert_eq!(
                reopened.write(&created).unwrap().derived,
                DerivedState::Fresh
            );
            assert_eq!(reopened.scan().unwrap(), vec![created]);
        }

        unsafe {
            std::env::remove_var(crate::infrastructure::paths::DATA_DIR_ENV);
        }
    }

    #[test]
    fn derived_toc_failures_commit_update_and_retry_same_identity() {
        let _guard = crate::test_support::process_env_guard();
        let logs = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::infrastructure::paths::DATA_DIR_ENV, logs.path());
        }

        for stage in [AtomicWriteStage::Write, AtomicWriteStage::Rename] {
            let tmp = tempfile::tempdir().unwrap();
            let store = MemoryStore::new(tmp.path());
            store.write(&memory("fact", "Old")).unwrap();
            let updated = memory("fact", "Updated");
            fail_next_atomic_write(&store.toc_path(), stage);

            let outcome = store.write(&updated).unwrap();
            assert_eq!(outcome.derived, DerivedState::RebuildNeeded);
            assert_eq!(store.scan().unwrap(), vec![updated.clone()]);

            let reopened = MemoryStore::new(tmp.path());
            assert_eq!(reopened.read("fact").unwrap(), Some(updated.clone()));
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
    fn derived_toc_failures_commit_remove_and_retry_without_double_delete() {
        let _guard = crate::test_support::process_env_guard();
        let logs = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::infrastructure::paths::DATA_DIR_ENV, logs.path());
        }

        for stage in [AtomicWriteStage::Write, AtomicWriteStage::Rename] {
            let tmp = tempfile::tempdir().unwrap();
            let store = MemoryStore::new(tmp.path());
            store.write(&memory("doomed", "Doomed")).unwrap();
            fail_next_atomic_write(&store.toc_path(), stage);

            let outcome = store.remove_with_outcome("doomed").unwrap();
            assert!(outcome.value);
            assert_eq!(outcome.derived, DerivedState::RebuildNeeded);
            assert!(store.scan().unwrap().is_empty());

            let reopened = MemoryStore::new(tmp.path());
            assert!(reopened.summaries().unwrap().is_empty());
            let retry = reopened.remove_with_outcome("doomed").unwrap();
            assert!(!retry.value);
            assert_eq!(retry.derived, DerivedState::Fresh);
        }

        unsafe {
            std::env::remove_var(crate::infrastructure::paths::DATA_DIR_ENV);
        }
    }

    #[test]
    fn source_atomic_failure_returns_error_without_mutating_memory() {
        for stage in [AtomicWriteStage::Write, AtomicWriteStage::Rename] {
            let tmp = tempfile::tempdir().unwrap();
            let store = MemoryStore::new(tmp.path());
            let source = store.dir().join("fact.md");
            fail_next_atomic_write(&source, stage);
            assert!(store.write(&memory("fact", "Created")).is_err());
            assert!(store.scan().unwrap().is_empty());

            store.write(&memory("fact", "Old")).unwrap();
            fail_next_atomic_write(&source, stage);
            assert!(store.write(&memory("fact", "Updated")).is_err());
            assert_eq!(store.read("fact").unwrap().unwrap().title, "Old");
        }
    }

    #[test]
    fn next_mutation_rebuilds_index_and_toc_when_derived_was_already_dirty() {
        let _guard = crate::test_support::process_env_guard();
        let logs = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var(crate::infrastructure::paths::DATA_DIR_ENV, logs.path());
        }
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path());
        store.write(&memory("one", "One")).unwrap();
        fail_next_atomic_write(&store.index_path(), AtomicWriteStage::Rename);
        assert_eq!(
            store.write(&memory("two", "Two")).unwrap().derived,
            DerivedState::RebuildNeeded
        );

        assert_eq!(
            store.write(&memory("three", "Three")).unwrap().derived,
            DerivedState::Fresh
        );
        let names: Vec<_> = store
            .summaries()
            .unwrap()
            .into_iter()
            .map(|summary| summary.name)
            .collect();
        assert_eq!(names, vec!["one", "three", "two"]);
        let toc = fs::read_to_string(store.toc_path()).unwrap();
        assert!(toc.contains("one.md"));
        assert!(toc.contains("two.md"));
        assert!(toc.contains("three.md"));

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
        let store = MemoryStore::new(tmp.path());
        store.write(&memory("one", "One")).unwrap();
        fail_next_atomic_write(&store.toc_path(), AtomicWriteStage::Rename);
        store.write(&memory("two", "Two")).unwrap();

        assert_eq!(
            store.remove_with_outcome("one").unwrap().derived,
            DerivedState::Fresh
        );
        fail_next_atomic_write(&store.toc_path(), AtomicWriteStage::Rename);
        store.write(&memory("three", "Three")).unwrap();
        fail_next_atomic_write(&store.index_path(), AtomicWriteStage::Rename);
        let names: Vec<_> = store
            .summaries()
            .unwrap()
            .into_iter()
            .map(|summary| summary.name)
            .collect();
        assert_eq!(names, vec!["three", "two"]);

        unsafe {
            std::env::remove_var(crate::infrastructure::paths::DATA_DIR_ENV);
        }
    }

    #[cfg(unix)]
    #[test]
    fn remove_reports_a_source_metadata_error_without_mutation() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path());
        fs::create_dir_all(store.dir()).unwrap();
        let path = store.dir().join("loop.md");
        symlink("loop.md", &path).unwrap();

        assert!(store.remove("loop").is_err());
        assert!(path.symlink_metadata().unwrap().file_type().is_symlink());
    }
}

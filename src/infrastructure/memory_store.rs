//! Persistence for a repository's agent memories.
//!
//! Memories live as `<name>.md` files under `<repo>/.usagi/memory/`, each a
//! frontmatter markdown document (see [`crate::domain::memory`]). The markdown
//! files are the source of truth. Two derived artifacts sit alongside them:
//!
//! - `MEMORY.md` — a human/agent-facing table of contents (one line per memory)
//!   meant to be loaded into context at the start of a session. It is committed
//!   and shared, like the memory files themselves.
//! - `index.json` — a metadata cache that speeds up listings. It is a local,
//!   rebuildable cache (kept out of git by `usagi init`'s `.gitignore` rules) and
//!   is never relied upon for correctness, only for speed.
//!
//! Both derived files are rebuilt from the markdown files whenever they are
//! missing or unreadable.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::domain::memory::{Memory, MemorySummary};
use crate::infrastructure::json_file;
use crate::infrastructure::repo_paths::STATE_DIR;
use crate::infrastructure::store_lock::StoreLock;

const MEMORY_DIR_NAME: &str = "memory";
/// Filename of the derived metadata cache. Kept out of git by the rules in
/// [`crate::infrastructure::gitignore`], which a test there cross-checks against
/// this constant.
pub(crate) const INDEX_FILE: &str = "index.json";
const TOC_FILE: &str = "MEMORY.md";
const FILE_FORMAT_VERSION: u32 = 1;

/// On-disk shape of `index.json`.
#[derive(Debug, Serialize, Deserialize)]
struct IndexFile {
    version: u32,
    memories: Vec<MemorySummary>,
}

/// The on-disk filename for a memory `name`, rejecting anything that is not a
/// single safe path component so a name can never escape the memory directory.
///
/// Defense in depth: the usecase layer already slugifies names to
/// ASCII-alphanumeric before they reach the store (see
/// [`crate::usecase::memory`]), so a separator or `..` only arrives through a
/// programming error — but [`MemoryStore`] is public, and `read`/`remove` would
/// otherwise turn `../../etc/passwd` into a read or delete outside `.usagi/`.
/// Mirrors the component check in [`crate::infrastructure::markdown_file`].
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
    dir: PathBuf,
}

impl MemoryStore {
    /// Open the memory store for the repository at `repo_root`.
    pub fn new(repo_root: impl AsRef<Path>) -> Self {
        Self {
            dir: repo_root.as_ref().join(STATE_DIR).join(MEMORY_DIR_NAME),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn index_path(&self) -> PathBuf {
        self.dir.join(INDEX_FILE)
    }

    pub fn toc_path(&self) -> PathBuf {
        self.dir.join(TOC_FILE)
    }

    /// Acquire this store's cross-process write lock, blocking until it is free.
    ///
    /// Hold the returned guard across a whole read-modify-write that must be
    /// atomic with respect to other processes — e.g. the upsert in
    /// [`crate::usecase::memory::save`], which reads the existing memory (to
    /// preserve its `created_at`) and then writes. The [`write`](Self::write) /
    /// [`remove`](Self::remove) entry points take the lock themselves; pass the
    /// guard to [`write_locked`](Self::write_locked) when you already hold it.
    pub fn lock(&self) -> Result<StoreLock> {
        StoreLock::acquire(&self.dir)
    }

    /// Read and parse every memory markdown file, sorted by name.
    pub fn scan(&self) -> Result<Vec<Memory>> {
        use rayon::prelude::*;

        let mut memories: Vec<Memory> = self
            .memory_files()?
            .into_par_iter()
            .map(|path| {
                let text = fs::read_to_string(&path)
                    .context(format!("failed to read {}", path.display()))?;
                Memory::from_markdown(&text)
                    .with_context(|| format!("failed to parse {}", path.display()))
            })
            .collect::<Result<Vec<_>>>()?;

        memories.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(memories)
    }

    /// Paths of every memory markdown file in the directory (the `MEMORY.md` table
    /// of contents and any non-`.md` files excluded). Empty when the directory
    /// does not exist.
    fn memory_files(&self) -> Result<Vec<PathBuf>> {
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
            if is_memory_file(&path) {
                files.push(path);
            }
        }
        Ok(files)
    }

    /// Read a single memory by name, or `None` if it does not exist.
    pub fn read(&self, name: &str) -> Result<Option<Memory>> {
        let path = self.dir.join(memory_file_name(name)?);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context(format!("failed to read {}", path.display())),
        };
        let memory = Memory::from_markdown(&text)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        Ok(Some(memory))
    }

    /// Write `memory` to disk and refresh the derived files, taking the store
    /// lock for the duration so concurrent writers serialise (the derived
    /// `index.json` / `MEMORY.md` are rebuilt by scanning the whole directory, so
    /// concurrent writes could otherwise commit a stale rebuild).
    pub fn write(&self, memory: &Memory) -> Result<()> {
        let lock = self.lock()?;
        self.write_locked(&lock, memory)
    }

    /// Like [`write`](Self::write) but assumes the caller already holds this
    /// store's [`lock`](Self::lock). Use this to keep the upsert read and the
    /// write inside one lock acquisition (see [`crate::usecase::memory::save`]).
    pub fn write_locked(&self, _lock: &StoreLock, memory: &Memory) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .context(format!("failed to create {}", self.dir.display()))?;

        let target = self.dir.join(memory.file_name());
        json_file::write_text_atomic(&target, &memory.to_markdown())?;
        self.rebuild_derived()?;
        Ok(())
    }

    /// Remove the memory with `name`, returning whether anything was deleted, then
    /// refresh the derived files. Takes the store lock for the duration.
    pub fn remove(&self, name: &str) -> Result<bool> {
        let _lock = self.lock()?;
        let path = self.dir.join(memory_file_name(name)?);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e).context(format!("failed to remove {}", path.display())),
        }
        self.rebuild_derived()?;
        Ok(true)
    }

    /// Metadata summaries for every memory.
    ///
    /// Uses `index.json` when it is present and parseable; otherwise it rebuilds
    /// the derived files from the markdown files (self-healing on a missing or
    /// corrupt cache).
    pub fn summaries(&self) -> Result<Vec<MemorySummary>> {
        match self.load_index()? {
            Some(index) => Ok(index.memories),
            None => self.rebuild_derived(),
        }
    }

    /// Load `index.json`, returning `None` when it is missing or unreadable (so the
    /// caller falls back to rebuilding from the markdown files).
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

    /// Rebuild `index.json` and `MEMORY.md` from the markdown files and return the
    /// summaries.
    fn rebuild_derived(&self) -> Result<Vec<MemorySummary>> {
        let summaries: Vec<MemorySummary> = self.scan()?.iter().map(Memory::summary).collect();
        if summaries.is_empty() && !self.dir.exists() {
            // Nothing stored and no directory yet: don't create files eagerly.
            return Ok(summaries);
        }
        fs::create_dir_all(&self.dir)
            .context(format!("failed to create {}", self.dir.display()))?;

        let index = IndexFile {
            version: FILE_FORMAT_VERSION,
            memories: summaries.clone(),
        };
        // The canonical "pretty JSON + trailing newline, written atomically" path
        // lives in `json_file::write_atomic`; reuse it for the index. `MEMORY.md`
        // is hand-rolled markdown, so it stays on `write_text_atomic`.
        json_file::write_atomic(&self.dir, &self.index_path(), &index)?;
        json_file::write_text_atomic(&self.toc_path(), &render_toc(&summaries))?;
        Ok(summaries)
    }
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
        out.push_str(&format!(
            "- [{}]({}) — {}\n",
            s.title,
            s.file,
            s.kind.as_str()
        ));
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
        // Defense in depth: even though callers slugify, the public store must
        // not let a `..`-bearing or separator-bearing name escape the memory
        // directory.
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
        }
        // A plain single-component name is still accepted (here: simply absent).
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
        // Removing again reports nothing was deleted.
        assert!(!store.remove("doomed").unwrap());
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
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path());
        store.write(&memory("one", "One")).unwrap();
        fs::write(store.index_path(), "{ not json").unwrap();

        let summaries = store.summaries().unwrap();
        assert_eq!(summaries.len(), 1);
        let text = fs::read_to_string(store.index_path()).unwrap();
        assert!(text.contains("\"version\": 1"));
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
    fn scan_errors_when_the_memory_path_is_not_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".usagi")).unwrap();
        fs::write(tmp.path().join(".usagi/memory"), "not a dir").unwrap();
        let store = MemoryStore::new(tmp.path());

        assert!(store
            .scan()
            .unwrap_err()
            .to_string()
            .contains("failed to read"));
    }

    #[test]
    fn summaries_error_when_the_index_is_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path());
        store.write(&memory("one", "One")).unwrap();
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
        // A directory where the memory file should be makes reading it fail.
        fs::create_dir(store.dir().join("broken.md")).unwrap();

        assert!(store
            .read("broken")
            .unwrap_err()
            .to_string()
            .contains("failed to read"));
    }

    #[test]
    fn remove_errors_when_the_path_is_not_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path());
        fs::create_dir_all(store.dir()).unwrap();
        // A directory at the memory path makes remove_file fail with a non-NotFound
        // error.
        fs::create_dir(store.dir().join("weird.md")).unwrap();

        assert!(store
            .remove("weird")
            .unwrap_err()
            .to_string()
            .contains("failed to remove"));
    }

    #[test]
    fn the_lock_file_is_not_picked_up_as_a_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path());
        store.write(&memory("one", "One")).unwrap();
        // Acquiring the lock creates the `.lock` file inside the store dir; it
        // must never be parsed as a memory or counted by scans.
        let _guard = store.lock().unwrap();
        assert!(store.dir().join(".lock").is_file());
        assert_eq!(store.scan().unwrap().len(), 1);
    }

    #[test]
    fn toc_and_index_are_ignored_by_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path());
        store.write(&memory("one", "One")).unwrap();
        // A stray non-markdown file, the index, and MEMORY.md must not be parsed.
        fs::write(store.dir().join("README.txt"), "ignore me").unwrap();

        assert_eq!(store.scan().unwrap().len(), 1);
    }
}

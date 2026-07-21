//! Shared persistence for markdown-source stores with a derived `index.json`.
//!
//! [`MarkdownStore`] owns the common mechanics for stores whose source of truth
//! is a directory of frontmatter markdown files and whose JSON index is only a
//! rebuildable metadata cache. Store-specific code supplies parsing, file/key
//! derivation, summary extraction, the on-disk index shape, and any extra
//! derived artifact such as memory's `MEMORY.md` table of contents.

use std::fs;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use sha2::{Digest, Sha256};

use crate::infrastructure::error_log::ErrorLog;
use crate::infrastructure::json_file;
use crate::infrastructure::store_lock::StoreLock;

/// Filename of the derived metadata cache shared by markdown-backed stores.
/// Kept out of git by the rules in [`crate::infrastructure::gitignore`].
pub(crate) const INDEX_FILE: &str = "index.json";
pub(crate) const DIRTY_FILE: &str = ".derived-dirty";

/// State of rebuildable files after a source-of-truth mutation committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DerivedState {
    Fresh,
    RebuildNeeded,
}

/// Successful source mutation together with the state of its derived files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationOutcome<T> {
    pub value: T,
    pub derived: DerivedState,
}

impl<T> MutationOutcome<T> {
    pub const fn new(value: T, derived: DerivedState) -> Self {
        Self { value, derived }
    }
}

/// Store-specific behavior for one kind of markdown entry.
pub(crate) trait MarkdownEntry: Sync {
    type Entry: Send;
    type Summary: Clone + Serialize + DeserializeOwned + Send + Sync;
    type Key: Ord + Clone;
    type IndexFile: DeserializeOwned;
    type IndexFileRef<'a>: Serialize
    where
        Self: 'a;

    /// Human-readable singular name used in error-log messages.
    const NAME: &'static str;

    fn is_entry_file(path: &Path) -> bool;
    fn parse_markdown(text: &str) -> Result<Self::Entry>;
    fn to_markdown(entry: &Self::Entry) -> String;
    fn file_name(entry: &Self::Entry) -> Result<String>;
    fn key(entry: &Self::Entry) -> Self::Key;
    fn key_from_summary(summary: &Self::Summary) -> Self::Key;
    fn key_from_path(path: &Path) -> Option<Self::Key>;
    fn summary(entry: &Self::Entry) -> Self::Summary;
    fn sort_entries(entries: &mut Vec<Self::Entry>);
    fn index_parts(index: Self::IndexFile) -> (Option<u32>, Option<String>, Vec<Self::Summary>);
    fn index_file_ref<'a>(
        summaries: &'a [Self::Summary],
        source_fingerprint: &'a str,
    ) -> Self::IndexFileRef<'a>;

    /// Write extra derived files after `index.json`. Stores without such files
    /// keep only the cache.
    fn write_extra_derived(_dir: &Path, _summaries: &[Self::Summary]) -> Result<()> {
        Ok(())
    }
}

struct MarkdownIndex<E: MarkdownEntry> {
    summaries: Vec<E::Summary>,
    source_fingerprint: String,
}

/// Common markdown + derived-index persistence rooted at one store directory.
pub(crate) struct MarkdownStore<E: MarkdownEntry> {
    dir: PathBuf,
    _entry: PhantomData<E>,
}

impl<E: MarkdownEntry> MarkdownStore<E> {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            _entry: PhantomData,
        }
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn index_path(&self) -> PathBuf {
        self.dir.join(INDEX_FILE)
    }

    fn dirty_path(&self) -> PathBuf {
        self.dir.join(DIRTY_FILE)
    }

    /// Read and parse every entry markdown file, sorted by its store-specific key.
    pub(crate) fn scan(&self) -> Result<Vec<E::Entry>> {
        use rayon::prelude::*;

        let mut entries: Vec<E::Entry> = self
            .entry_files()?
            .into_par_iter()
            .map(|path| self.read_existing_path(&path))
            .collect::<Result<Vec<_>>>()?;
        E::sort_entries(&mut entries);
        Ok(entries)
    }

    /// Like [`scan`](Self::scan), but records unreadable/unparseable files to the
    /// error log and skips them so one corrupt sibling cannot break listings or
    /// cache rebuilds.
    pub(crate) fn scan_lenient(&self) -> Result<Vec<E::Entry>> {
        use rayon::prelude::*;

        let parsed: Vec<(PathBuf, Result<E::Entry>)> = self
            .entry_files()?
            .into_par_iter()
            .map(|path| {
                let entry = self.read_existing_path(&path);
                (path, entry)
            })
            .collect();

        let mut entries = Vec::with_capacity(parsed.len());
        for (path, entry) in parsed {
            match entry {
                Ok(entry) => entries.push(entry),
                Err(e) => ErrorLog::record(&format!(
                    "skipping unparseable {} file {}: {e:#}",
                    E::NAME,
                    path.display()
                )),
            }
        }
        E::sort_entries(&mut entries);
        Ok(entries)
    }

    /// Paths of every source markdown file in the directory. Empty when the
    /// directory does not exist.
    pub(crate) fn entry_files(&self) -> Result<Vec<PathBuf>> {
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
            if E::is_entry_file(&path) {
                files.push(path);
            }
        }
        Ok(files)
    }

    pub(crate) fn files_for_key(&self, key: &E::Key) -> Result<Vec<PathBuf>> {
        Ok(self
            .entry_files()?
            .into_iter()
            .filter(|path| E::key_from_path(path).as_ref() == Some(key))
            .collect())
    }

    pub(crate) fn read_existing_path(&self, path: &Path) -> Result<E::Entry> {
        let text =
            fs::read_to_string(path).context(format!("failed to read {}", path.display()))?;
        E::parse_markdown(&text).with_context(|| format!("failed to parse {}", path.display()))
    }

    pub(crate) fn write_markdown(&self, entry: &E::Entry) -> Result<PathBuf> {
        fs::create_dir_all(&self.dir)
            .context(format!("failed to create {}", self.dir.display()))?;
        let target = self.dir.join(E::file_name(entry)?);
        json_file::write_text_atomic(&target, &E::to_markdown(entry))?;
        Ok(target)
    }

    /// Schedule a rebuild durably before changing source Markdown.
    pub(crate) fn mark_derived_dirty(&self) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .context(format!("failed to create {}", self.dir.display()))?;
        json_file::write_text_atomic(&self.dirty_path(), "rebuild from markdown\n")
    }

    /// Finish an already committed mutation without turning derived failure
    /// into an error that callers might retry as an unapplied source mutation.
    pub(crate) fn finish_committed<T>(&self, value: T, refresh: Result<()>) -> MutationOutcome<T> {
        match refresh.and_then(|()| self.clear_derived_dirty()) {
            Ok(()) => MutationOutcome::new(value, DerivedState::Fresh),
            Err(error) => {
                ErrorLog::record(&format!(
                    "{} source committed but derived refresh failed; rebuild remains scheduled: {error:#}",
                    E::NAME
                ));
                MutationOutcome::new(value, DerivedState::RebuildNeeded)
            }
        }
    }

    pub(crate) fn repair_derived_locked(&self) -> Result<()> {
        if !self.derived_is_dirty() {
            return Ok(());
        }
        self.rebuild_derived_locked()?;
        self.clear_derived_dirty()
    }

    pub(crate) fn derived_is_dirty(&self) -> bool {
        self.dirty_path().exists()
    }

    /// Whether the current cache belongs to the current source revision. The
    /// caller holds the store lock so a following mutation can safely choose an
    /// incremental update only from this exact base.
    pub(crate) fn derived_is_fresh_locked(&self) -> Result<bool> {
        Ok(self.load_fresh_index()?.is_some())
    }

    fn clear_derived_dirty(&self) -> Result<()> {
        match fs::remove_file(self.dirty_path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context(format!(
                "failed to clear derived rebuild marker {}",
                self.dirty_path().display()
            )),
        }
    }

    /// Patch the derived cache/extra files after `entry` was written. Falls back
    /// to a full rebuild when the cache is missing or unreadable.
    pub(crate) fn reindex_after_write(&self, entry: &E::Entry) -> Result<()> {
        let Some(mut summaries) = self.load_index()?.map(|index| index.summaries) else {
            return self.rebuild_derived_locked().map(|_| ());
        };
        let key = E::key(entry);
        let summary = E::summary(entry);
        match summaries.binary_search_by(|s| E::key_from_summary(s).cmp(&key)) {
            Ok(pos) => summaries[pos] = summary,
            Err(pos) => summaries.insert(pos, summary),
        }
        self.write_derived_locked(&summaries)
    }

    /// Patch the derived cache/extra files after `key` was removed. Falls back to
    /// a full rebuild when the cache is missing or unreadable.
    pub(crate) fn reindex_after_remove(&self, key: &E::Key) -> Result<()> {
        let Some(mut summaries) = self.load_index()?.map(|index| index.summaries) else {
            return self.rebuild_derived_locked().map(|_| ());
        };
        if let Ok(pos) = summaries.binary_search_by(|s| E::key_from_summary(s).cmp(key)) {
            summaries.remove(pos);
            self.write_derived_locked(&summaries)?;
        }
        Ok(())
    }

    /// Metadata summaries for every entry. The index is used only when it is
    /// parseable and fresh relative to the markdown files.
    pub(crate) fn summaries(&self) -> Result<Vec<E::Summary>> {
        if self.directory_is_missing()? {
            return Ok(Vec::new());
        }
        let _lock = StoreLock::acquire(&self.dir)?;
        match self.load_fresh_index()? {
            Some(index) => Ok(index.summaries),
            None => self.rebuild_derived_locked(),
        }
    }

    #[cfg(test)]
    pub(crate) fn summaries_with_rebuild_hook(
        &self,
        after_scan: impl FnOnce(),
    ) -> Result<Vec<E::Summary>> {
        let _lock = StoreLock::acquire(&self.dir)?;
        self.rebuild_derived_locked_with_hook(after_scan)
    }

    fn directory_is_missing(&self) -> Result<bool> {
        match fs::metadata(&self.dir) {
            Ok(_) => Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(error).context(format!("failed to inspect {}", self.dir.display())),
        }
    }

    /// Write `index.json` and any store-specific derived artifact.
    fn write_derived_locked(&self, summaries: &[E::Summary]) -> Result<()> {
        self.write_index_locked(summaries)?;
        E::write_extra_derived(&self.dir, summaries)
    }

    /// Write the sorted summaries to `index.json` as a rebuildable cache.
    #[cfg(test)]
    pub(crate) fn write_index(&self, summaries: &[E::Summary]) -> Result<()> {
        let _lock = StoreLock::acquire(&self.dir)?;
        self.write_index_locked(summaries)
    }

    fn write_index_locked(&self, summaries: &[E::Summary]) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .context(format!("failed to create {}", self.dir.display()))?;
        let source_fingerprint = self.source_fingerprint()?;
        let index = E::index_file_ref(summaries, &source_fingerprint);
        json_file::write_atomic_cache(&self.dir, &self.index_path(), &index)
    }

    /// Rebuild all derived files from the markdown source of truth.
    pub(crate) fn rebuild_derived_locked(&self) -> Result<Vec<E::Summary>> {
        self.rebuild_derived_locked_with_hook(|| {})
    }

    fn rebuild_derived_locked_with_hook(
        &self,
        after_scan: impl FnOnce(),
    ) -> Result<Vec<E::Summary>> {
        let summaries: Vec<E::Summary> = self.scan_lenient()?.iter().map(E::summary).collect();
        after_scan();
        if summaries.is_empty() && !self.dir.exists() {
            return Ok(summaries);
        }
        self.write_derived_locked(&summaries)?;
        Ok(summaries)
    }

    /// Read summaries directly from source, bypassing an unusable derived cache.
    pub(crate) fn source_summaries(&self) -> Result<Vec<E::Summary>> {
        Ok(self.scan_lenient()?.iter().map(E::summary).collect())
    }

    /// Load `index.json` only when it is fresh relative to the markdown files.
    /// A missing, unreadable, corrupt, legacy, or source-mismatched cache returns
    /// `None` so callers rebuild from the markdown source of truth.
    fn load_fresh_index(&self) -> Result<Option<MarkdownIndex<E>>> {
        let Some(index) = self.load_index()? else {
            return Ok(None);
        };
        if self.source_fingerprint()? != index.source_fingerprint {
            return Ok(None);
        }
        Ok(Some(index))
    }

    /// Stable identity of the complete source set. Paths are framed before file
    /// bytes so renames and same-size replacements cannot alias each other.
    fn source_fingerprint(&self) -> Result<String> {
        let mut files = self.entry_files()?;
        files.sort();
        let mut hasher = Sha256::new();
        for path in files {
            let name = path
                .file_name()
                .expect("directory entries always have a filename")
                .as_encoded_bytes();
            let bytes = fs::read(&path).context(format!("failed to read {}", path.display()))?;
            hasher.update((name.len() as u64).to_le_bytes());
            hasher.update(name);
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        }
        Ok(format!("sha256:{:x}", hasher.finalize()))
    }

    /// Load `index.json`, returning `None` when it is missing or corrupt. A
    /// present-but-corrupt cache is recoverable but logged before rebuilding.
    fn load_index(&self) -> Result<Option<MarkdownIndex<E>>> {
        let text = match fs::read_to_string(self.index_path()) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(e).context(format!("failed to read {}", self.index_path().display()))
            }
        };
        match serde_json::from_str::<E::IndexFile>(&text) {
            Ok(index) => {
                let (version, source_fingerprint, summaries) = E::index_parts(index);
                if version != Some(json_file::FILE_FORMAT_VERSION) {
                    return Ok(None);
                }
                let Some(source_fingerprint) = source_fingerprint else {
                    return Ok(None);
                };
                Ok(Some(MarkdownIndex {
                    summaries,
                    source_fingerprint,
                }))
            }
            Err(e) => {
                ErrorLog::record(&format!(
                    "{} index {} is corrupt; rebuilding from markdown: {e}",
                    E::NAME,
                    self.index_path().display()
                ));
                Ok(None)
            }
        }
    }
}

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

use crate::infrastructure::error_log::ErrorLog;
use crate::infrastructure::json_file;

/// Filename of the derived metadata cache shared by markdown-backed stores.
/// Kept out of git by the rules in [`crate::infrastructure::gitignore`].
pub(crate) const INDEX_FILE: &str = "index.json";

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
    fn summaries_from_index(index: Self::IndexFile) -> Vec<Self::Summary>;
    fn index_file_ref(summaries: &[Self::Summary]) -> Self::IndexFileRef<'_>;

    /// Write extra derived files after `index.json`. Stores without such files
    /// keep only the cache.
    fn write_extra_derived(_dir: &Path, _summaries: &[Self::Summary]) -> Result<()> {
        Ok(())
    }
}

struct MarkdownIndex<E: MarkdownEntry> {
    summaries: Vec<E::Summary>,
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

    /// Patch the derived cache/extra files after `entry` was written. Falls back
    /// to a full rebuild when the cache is missing or unreadable.
    pub(crate) fn reindex_after_write(&self, entry: &E::Entry) -> Result<()> {
        let Some(mut summaries) = self.load_index()?.map(|index| index.summaries) else {
            return self.rebuild_derived().map(|_| ());
        };
        let key = E::key(entry);
        let summary = E::summary(entry);
        match summaries.binary_search_by(|s| E::key_from_summary(s).cmp(&key)) {
            Ok(pos) => summaries[pos] = summary,
            Err(pos) => summaries.insert(pos, summary),
        }
        self.write_derived(&summaries)
    }

    /// Patch the derived cache/extra files after `key` was removed. Falls back to
    /// a full rebuild when the cache is missing or unreadable.
    pub(crate) fn reindex_after_remove(&self, key: &E::Key) -> Result<()> {
        let Some(mut summaries) = self.load_index()?.map(|index| index.summaries) else {
            return self.rebuild_derived().map(|_| ());
        };
        if let Ok(pos) = summaries.binary_search_by(|s| E::key_from_summary(s).cmp(key)) {
            summaries.remove(pos);
            self.write_derived(&summaries)?;
        }
        Ok(())
    }

    /// Metadata summaries for every entry. The index is used only when it is
    /// parseable and fresh relative to the markdown files.
    pub(crate) fn summaries(&self) -> Result<Vec<E::Summary>> {
        match self.load_fresh_index()? {
            Some(index) => Ok(index.summaries),
            None => self.rebuild_derived(),
        }
    }

    /// Write `index.json` and any store-specific derived artifact.
    pub(crate) fn write_derived(&self, summaries: &[E::Summary]) -> Result<()> {
        self.write_index(summaries)?;
        E::write_extra_derived(&self.dir, summaries)
    }

    /// Write the sorted summaries to `index.json` as a rebuildable cache.
    pub(crate) fn write_index(&self, summaries: &[E::Summary]) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .context(format!("failed to create {}", self.dir.display()))?;
        let index = E::index_file_ref(summaries);
        json_file::write_atomic_cache(&self.dir, &self.index_path(), &index)
    }

    /// Rebuild all derived files from the markdown source of truth.
    pub(crate) fn rebuild_derived(&self) -> Result<Vec<E::Summary>> {
        let summaries: Vec<E::Summary> = self.scan_lenient()?.iter().map(E::summary).collect();
        if summaries.is_empty() && !self.dir.exists() {
            return Ok(summaries);
        }
        self.write_derived(&summaries)?;
        Ok(summaries)
    }

    /// Load `index.json` only when it is fresh relative to the markdown files.
    /// A missing, unreadable, corrupt, count-mismatched, or older cache returns
    /// `None` so callers rebuild from the markdown source of truth.
    fn load_fresh_index(&self) -> Result<Option<MarkdownIndex<E>>> {
        let Ok(index_mtime) = fs::metadata(self.index_path()).and_then(|m| m.modified()) else {
            return Ok(None);
        };
        let Some(index) = self.load_index()? else {
            return Ok(None);
        };
        let files = self.entry_files()?;
        if files.len() != index.summaries.len() {
            return Ok(None);
        }
        for path in &files {
            let fresh = fs::metadata(path)
                .and_then(|m| m.modified())
                .is_ok_and(|mtime| mtime <= index_mtime);
            if !fresh {
                return Ok(None);
            }
        }
        Ok(Some(index))
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
            Ok(index) => Ok(Some(MarkdownIndex {
                summaries: E::summaries_from_index(index),
            })),
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

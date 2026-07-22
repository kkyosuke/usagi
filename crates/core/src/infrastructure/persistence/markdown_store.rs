//! Shared persistence for markdown-source stores with a derived `index.json`.
//!
//! [`MarkdownStore`] owns the common mechanics for stores whose source of truth
//! is a directory of frontmatter markdown files and whose JSON index is only a
//! rebuildable metadata cache. Store-specific code supplies parsing, filename
//! derivation, summary extraction, the on-disk index shape, and any extra
//! derived artifact such as memory's `MEMORY.md` table of contents.

use std::fs;
use std::io::Read;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

use super::json_file;
use crate::infrastructure::error_log::ErrorLog;
use crate::infrastructure::store::{DerivedState, MutationOutcome};

/// Filename of the derived metadata cache shared by markdown-backed stores.
/// Kept out of git by usagi's ignore rules.
pub(crate) const INDEX_FILE: &str = "index.json";
const DIRTY_FILE: &str = ".derived-dirty";
pub(crate) const INDEX_FORMAT_VERSION: u32 = 2;
const FINGERPRINT_ALGORITHM: &str = "sha256";

/// Store-specific behavior for one kind of markdown entry.
pub(crate) trait MarkdownEntry: Sync {
    type Entry: Send;
    type Summary: Clone + Serialize + DeserializeOwned + Send + Sync;
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
    fn summary(entry: &Self::Entry) -> Self::Summary;
    fn sort_entries(entries: &mut Vec<Self::Entry>);
    fn index_parts(index: Self::IndexFile) -> (u32, String, Vec<Self::Summary>);
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
    version: u32,
    source_fingerprint: String,
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

    /// Whether the source directory exists, distinguishing a genuinely absent
    /// path from a dangling final component or ancestor.
    pub(crate) fn source_dir_exists(&self) -> Result<bool> {
        match fs::metadata(&self.dir) {
            Ok(metadata) => {
                anyhow::ensure!(
                    metadata.is_dir(),
                    "markdown source path is not a directory: {}",
                    self.dir.display()
                );
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if path_is_genuinely_missing(&self.dir)? {
                    Ok(false)
                } else {
                    Err(error).context(format!("failed to inspect {}", self.dir.display()))
                }
            }
            Err(error) => Err(error).context(format!("failed to inspect {}", self.dir.display())),
        }
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
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if path_is_genuinely_missing(&self.dir)? {
                    return Ok(Vec::new());
                }
                return Err(e).context(format!("failed to read {}", self.dir.display()));
            }
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

    /// Compute a deterministic identity for the complete source set.
    ///
    /// Each entry contributes its UTF-8 file name and exact bytes, both framed
    /// with fixed-width lengths. Sorting names makes the result independent of
    /// directory iteration order. Reading bytes (rather than metadata) catches
    /// preserved/coarse mtimes and same-size edits.
    fn source_fingerprint(mut files: Vec<PathBuf>) -> Result<String> {
        files.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
        let mut hasher = Sha256::new();
        for path in files {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .context(format!("source filename is not UTF-8: {}", path.display()))?;
            let name = name.as_bytes();
            hasher.update((name.len() as u64).to_be_bytes());
            hasher.update(name);

            let mut file =
                fs::File::open(&path).context(format!("failed to read {}", path.display()))?;
            let length = file
                .metadata()
                .context(format!("failed to inspect {}", path.display()))?
                .len();
            hasher.update(length.to_be_bytes());
            let mut buffer = [0_u8; 16 * 1024];
            loop {
                let read = file
                    .read(&mut buffer)
                    .context(format!("failed to read {}", path.display()))?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
        }
        Ok(format!("{FINGERPRINT_ALGORITHM}:{:x}", hasher.finalize()))
    }

    // Takes `&self` for call-site consistency with the store's other methods
    // even though it only needs `path` and `E`.
    #[allow(clippy::unused_self)]
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

    /// Durably schedule a rebuild before changing source Markdown. A crash at
    /// any later point leaves enough information for the next reader to repair
    /// derived files from the source state that actually reached disk.
    pub(crate) fn mark_derived_dirty(&self) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .context(format!("failed to create {}", self.dir.display()))?;
        json_file::write_text_atomic(&self.dirty_path(), "rebuild from markdown\n")
    }

    /// Convert a derived refresh result into the outcome of an already
    /// committed source mutation. Derived errors are logged and retained as a
    /// dirty marker; they never masquerade as an unapplied source error.
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

    /// Repair a previously scheduled rebuild while the caller holds the store
    /// lock. Missing markers make this a no-op.
    pub(crate) fn repair_derived_locked(&self) -> Result<()> {
        if !self.dirty_path().exists() {
            return Ok(());
        }
        self.rebuild_derived()?;
        self.clear_derived_dirty()
    }

    pub(crate) fn derived_is_dirty(&self) -> bool {
        self.dirty_path().exists()
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

    /// Refresh derived files after a source write. Re-scan all sources so an
    /// unrelated manual edit cannot be hidden behind a newly stamped identity.
    pub(crate) fn reindex_after_write(&self, _entry: &E::Entry) -> Result<()> {
        self.rebuild_derived().map(|_| ())
    }

    /// Refresh derived files after a source removal under the same contract as
    /// [`reindex_after_write`](Self::reindex_after_write).
    pub(crate) fn reindex_after_remove(&self) -> Result<()> {
        self.rebuild_derived().map(|_| ())
    }

    /// Metadata summaries for every entry. The index is used only when it is
    /// parseable and fresh relative to the markdown files.
    pub(crate) fn summaries(&self) -> Result<Vec<E::Summary>> {
        match self.load_fresh_index()? {
            Some(index) => Ok(index.summaries),
            None => self.rebuild_derived(),
        }
    }

    /// Build summaries directly from source without publishing derived files.
    pub(crate) fn source_summaries(&self) -> Result<Vec<E::Summary>> {
        Ok(self.scan_lenient()?.iter().map(E::summary).collect())
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
        let fingerprint = Self::source_fingerprint(self.entry_files()?)?;
        let index = E::index_file_ref(summaries, &fingerprint);
        json_file::write_atomic_cache(&self.dir, &self.index_path(), &index)
    }

    /// Rebuild all derived files from the markdown source of truth.
    pub(crate) fn rebuild_derived(&self) -> Result<Vec<E::Summary>> {
        let summaries = self.source_summaries()?;
        if summaries.is_empty() && !self.dir.exists() {
            return Ok(summaries);
        }
        self.write_derived(&summaries)?;
        Ok(summaries)
    }

    /// Load `index.json` only when it is fresh relative to the markdown files.
    /// A missing, unreadable, corrupt, legacy, unknown-version, or identity-
    /// mismatched cache returns `None` so callers rebuild from markdown.
    fn load_fresh_index(&self) -> Result<Option<MarkdownIndex<E>>> {
        let Some(index) = self.load_index()? else {
            return Ok(None);
        };
        let files = self.entry_files()?;
        if index.version != INDEX_FORMAT_VERSION
            || !valid_fingerprint(&index.source_fingerprint)
            || Self::source_fingerprint(files)? != index.source_fingerprint
        {
            return Ok(None);
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
                return Err(e).context(format!("failed to read {}", self.index_path().display()));
            }
        };
        match serde_json::from_str::<E::IndexFile>(&text) {
            Ok(index) => {
                let (version, source_fingerprint, summaries) = E::index_parts(index);
                Ok(Some(MarkdownIndex {
                    version,
                    source_fingerprint,
                    summaries,
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

/// Return true only when a missing path reaches an existing real-directory
/// ancestor. Encountering a symlink or non-directory first is ambiguous (most
/// importantly, a dangling symlink) and must not be treated as an empty store.
fn path_is_genuinely_missing(path: &Path) -> Result<bool> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                return Ok(metadata.is_dir() && !metadata.file_type().is_symlink());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .context(format!("failed to inspect ancestor {}", ancestor.display()));
            }
        }
    }
    Ok(false)
}

fn valid_fingerprint(fingerprint: &str) -> bool {
    fingerprint.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

//! Shared helpers for the files usagi persists under its data directories.
//!
//! Every store treats a missing file as "no data yet" and writes through a temp
//! file + rename so a crash never leaves a half-written file behind. The temp
//! file's contents are flushed with `sync_all` (fsync) before the rename, so its
//! data survives a power loss / hard crash rather than only a process crash;
//! after the rename the parent directory is fsynced best-effort so the rename
//! itself is durable where the platform supports it (directory fsync is a no-op
//! or errors on some platforms such as Windows and is intentionally ignored).
//! JSON stores (`storage`) use [`read`] / [`write_atomic`]; the markdown stores
//! (`issue_store`, `memory_store`) use [`write_text_atomic`] for their
//! hand-rolled text. All share one per-writer-unique temp-name scheme so two
//! processes writing the same path never clobber each other.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Bumped per atomic write so each write within this process gets a distinct
/// temp file name; combined with the pid it is unique across processes too.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AtomicWriteStage {
    Write,
    Rename,
}

#[cfg(test)]
struct AtomicWriteFailpoint {
    path: PathBuf,
    stage: AtomicWriteStage,
}

#[cfg(test)]
thread_local! {
    static ATOMIC_WRITE_FAILPOINT: std::cell::RefCell<Option<AtomicWriteFailpoint>> = const {
        std::cell::RefCell::new(None)
    };
}

/// Fail the next matching atomic write in this test thread.
#[cfg(test)]
pub(crate) fn fail_next_atomic_write(path: &Path, stage: AtomicWriteStage) {
    ATOMIC_WRITE_FAILPOINT.with(|failpoint| {
        *failpoint.borrow_mut() = Some(AtomicWriteFailpoint {
            path: path.to_path_buf(),
            stage,
        });
    });
}

#[cfg(test)]
fn take_atomic_write_failpoint(path: &Path, stage: AtomicWriteStage) -> bool {
    ATOMIC_WRITE_FAILPOINT.with(|failpoint| {
        let matches = failpoint
            .borrow()
            .as_ref()
            .is_some_and(|failpoint| failpoint.path == path && failpoint.stage == stage);
        if matches {
            failpoint.borrow_mut().take();
        }
        matches
    })
}

/// A per-writer-unique temp path for `path`: the full target name with a
/// `.tmp.<pid>.<counter>` suffix appended.
///
/// A fixed `*.tmp` would let two processes writing the same path (e.g. the MCP
/// server and the TUI both editing one repo's `index.json`) clobber each other's
/// half-written temp before the rename. The pid keeps the name unique across
/// processes, the counter within one.
#[coverage(off)]
fn unique_tmp_path(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(
        ".tmp.{}.{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    PathBuf::from(tmp)
}

/// Write `bytes` to a unique temp file and rename it onto `path`. When `durable`,
/// the temp file's contents are fsynced before the rename and the parent
/// directory is fsynced after it, so the write survives a power loss; otherwise
/// only the atomic rename is guaranteed (a crash never exposes a half-written
/// file, but the latest bytes may not have reached disk yet).
///
/// The non-durable mode is for rebuildable derived *caches* (`index.json`): they
/// are never relied on for correctness — a stale or missing cache self-heals from
/// the source-of-truth markdown on the next read — so paying an fsync on every
/// write to make them power-loss durable is wasted IO in the store lock's hot path.
/// Source-of-truth files (JSON state, memory/issue markdown) stay durable.
#[coverage(off)]
fn write_atomically(path: &Path, bytes: &[u8], durable: bool) -> Result<()> {
    let tmp = unique_tmp_path(path);
    // Clean up the temp file on any failure after it is created. The write
    // (write_all / sync_all) or the rename can fail — rename especially
    // (EXDEV/cross-device, ENOSPC, EACCES) — and without this each failed write
    // leaves an orphaned `*.tmp.<pid>.<n>` behind, so a recurring failure litters
    // the data dir without bound. The rename is still atomic, so a failed write
    // never replaces the existing good file; this only removes the dead temp.
    let result = write_atomically_inner(&tmp, path, bytes, durable);
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// The body of [`write_atomically`]: create-write(-sync) the temp file, rename it
/// onto `path`, then (when durable) fsync the parent. Split out so the caller can
/// remove the temp file on any error this returns.
#[coverage(off)]
fn write_atomically_inner(tmp: &Path, path: &Path, bytes: &[u8], durable: bool) -> Result<()> {
    #[cfg(test)]
    if take_atomic_write_failpoint(path, AtomicWriteStage::Write) {
        anyhow::bail!("injected atomic write failure for {}", path.display());
    }
    {
        let mut file =
            fs::File::create(tmp).context(format!("failed to create {}", tmp.display()))?;
        file.write_all(bytes)
            .context(format!("failed to write {}", tmp.display()))?;
        if durable {
            file.sync_all()
                .context(format!("failed to flush {}", tmp.display()))?;
        }
    }
    #[cfg(test)]
    if take_atomic_write_failpoint(path, AtomicWriteStage::Rename) {
        anyhow::bail!("injected atomic rename failure for {}", path.display());
    }
    fs::rename(tmp, path).context(format!("failed to replace {}", path.display()))?;
    if durable {
        fsync_parent_dir(path);
    }
    Ok(())
}

/// Best-effort fsync of `path`'s parent directory so a preceding rename's
/// directory entry is durable.
///
/// Directory fsync is what makes a rename survive power loss, but it is a no-op
/// or returns an error on some platforms (e.g. Windows) and may fail if the
/// parent cannot be opened. Such failures must not fail an otherwise-successful
/// write, so every error here is intentionally swallowed.
#[coverage(off)]
fn fsync_parent_dir(path: &Path) {
    let Some(parent) = path.parent() else { return };
    let Ok(dir) = fs::File::open(parent) else {
        return;
    };
    let _ = dir.sync_all();
}

/// Read and deserialize the JSON file at `path`, returning `None` if it does
/// not exist.
///
/// # Errors
///
/// Returns an error when the file exists but cannot be read, or when its
/// contents are not valid JSON for `T`.
#[coverage(off)]
pub fn read<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context(format!("failed to read {}", path.display())),
    };
    let value =
        serde_json::from_str(&text).context(format!("failed to parse {}", path.display()))?;
    Ok(Some(value))
}

/// Serialize `value` to pretty JSON and write it durably and atomically to `path`
/// (temp file + fsync + rename), creating `dir` (the directory that contains
/// `path`) first. For source-of-truth JSON; a rebuildable cache uses
/// [`write_atomic_cache`].
///
/// # Errors
///
/// Returns an error when `dir` cannot be created, `value` cannot be serialized,
/// or the temp file cannot be written or renamed onto `path`.
#[coverage(off)]
pub fn write_atomic<T: Serialize>(dir: &Path, path: &Path, value: &T) -> Result<()> {
    write_json(dir, path, value, true)
}

/// Like [`write_atomic`] but for a rebuildable derived cache: the write is atomic
/// (temp file + rename) but not fsynced, so it does not pay the durability cost of
/// a source-of-truth file. A power loss may lose the latest cache bytes; the cache
/// self-heals from the markdown source of truth on the next read.
///
/// # Errors
///
/// Returns an error when `dir` cannot be created, `value` cannot be serialized,
/// or the temp file cannot be written or renamed onto `path`.
#[coverage(off)]
pub fn write_atomic_cache<T: Serialize>(dir: &Path, path: &Path, value: &T) -> Result<()> {
    write_json(dir, path, value, false)
}

#[coverage(off)]
fn write_json<T: Serialize>(dir: &Path, path: &Path, value: &T, durable: bool) -> Result<()> {
    fs::create_dir_all(dir).context(format!("failed to create {}", dir.display()))?;
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    write_atomically(path, text.as_bytes(), durable)
}

/// Write `text` to `path` durably and atomically (per-writer-unique temp file +
/// fsync + rename) so a crash never leaves a half-written file behind and two
/// processes writing the same path never clobber each other's temp. Unlike
/// [`write_atomic`], `text` is written verbatim and the parent directory is
/// assumed to exist already — the markdown stores create it when they set up their
/// data dir.
///
/// # Errors
///
/// Returns an error when the temp file cannot be written or renamed onto `path`.
#[coverage(off)]
pub fn write_text_atomic(path: &Path, text: &str) -> Result<()> {
    write_atomically(path, text.as_bytes(), true)
}

/// The on-disk format version stamped onto every versioned store file
/// (`storage`'s `workspaces.json` and the issue/memory `index.json` derived
/// caches). Bumped only on an incompatible on-disk format change; the single
/// source of truth for the envelope's `version` field so no store carries its
/// own copy.
pub const FILE_FORMAT_VERSION: u32 = 1;

/// The on-disk envelope shared by every versioned store file: a `version` plus
/// the flattened payload (`{ "version": N, <payload…> }`). The write side
/// borrows the payload (so callers never clone it into an owned wrapper); the
/// read side accepts and ignores the `version`, which is reserved for future
/// format migrations.
#[derive(Serialize)]
struct VersionedRef<'a, T: ?Sized> {
    version: u32,
    #[serde(flatten)]
    inner: &'a T,
}

#[derive(Deserialize)]
struct Versioned<T> {
    // Accepted so the envelope round-trips; not read today, but reserved for a
    // future format migration that needs to branch on it.
    #[serde(default)]
    #[allow(dead_code)]
    version: u32,
    #[serde(flatten)]
    inner: T,
}

/// Read the payload from a versioned JSON file — the `{ "version": N, <payload…> }`
/// envelope the stores write — returning `None` when the file does not exist.
/// The envelope's `version` is accepted and ignored.
///
/// # Errors
///
/// Returns an error when the file exists but cannot be read or parsed.
#[coverage(off)]
pub fn read_versioned<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    Ok(read::<Versioned<T>>(path)?.map(|v| v.inner))
}

/// Serialize `payload` and write it atomically to `path` as a versioned JSON
/// file, stamping the current [`FILE_FORMAT_VERSION`]. The payload is serialized
/// by reference, so the caller never clones it into an owned envelope struct.
///
/// # Errors
///
/// Returns an error when `dir` cannot be created, `payload` cannot be serialized,
/// or the temp file cannot be written or renamed onto `path`.
#[coverage(off)]
pub fn write_versioned<T: Serialize + ?Sized>(dir: &Path, path: &Path, payload: &T) -> Result<()> {
    write_atomic(
        dir,
        path,
        &VersionedRef {
            version: FILE_FORMAT_VERSION,
            inner: payload,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_text_atomic_writes_and_replaces_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        write_text_atomic(&path, "hello\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello\n");

        // A second write replaces the file in place and leaves no temp behind.
        write_text_atomic(&path, "world\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "world\n");
        let leftover: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftover.is_empty(), "temp files left behind: {leftover:?}");
    }

    #[test]
    fn write_atomic_round_trips_json_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        let value = vec!["a".to_string(), "b".to_string()];
        write_atomic(dir.path(), &path, &value).unwrap();

        let read_back: Option<Vec<String>> = read(&path).unwrap();
        assert_eq!(read_back, Some(value.clone()));
        // Pretty JSON plus a trailing newline reaches disk after the fsync.
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.ends_with('\n'));

        // A second write replaces in place and leaves no temp behind.
        let value2 = vec!["c".to_string()];
        write_atomic(dir.path(), &path, &value2).unwrap();
        let read_back2: Option<Vec<String>> = read(&path).unwrap();
        assert_eq!(read_back2, Some(value2));
        let leftover: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftover.is_empty(), "temp files left behind: {leftover:?}");
    }

    #[test]
    fn write_removes_the_temp_file_when_the_rename_fails() {
        let dir = tempfile::tempdir().unwrap();
        // The target path is an existing, non-empty directory, so the final
        // rename(temp, path) fails *after* the temp file has been created and
        // synced — exercising the failure-cleanup path.
        let path = dir.path().join("target");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("child"), "x").unwrap();

        assert!(write_text_atomic(&path, "data").is_err());

        // The dead temp file was removed rather than orphaned in the data dir.
        let leftover: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftover.is_empty(), "temp files left behind: {leftover:?}");
    }

    #[test]
    fn write_atomic_cache_round_trips_json_without_fsync() {
        // The cache variant skips the temp-file fsync and the parent-dir fsync
        // (the non-durable branch of `write_atomically`) but still writes atomically
        // through a temp file + rename, so it round-trips and leaves no temp behind.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        let value = vec!["a".to_string(), "b".to_string()];
        write_atomic_cache(dir.path(), &path, &value).unwrap();

        let read_back: Option<Vec<String>> = read(&path).unwrap();
        assert_eq!(read_back, Some(value));
        assert!(fs::read_to_string(&path).unwrap().ends_with('\n'));

        // A second cache write replaces in place and leaves no temp behind.
        let value2 = vec!["c".to_string()];
        write_atomic_cache(dir.path(), &path, &value2).unwrap();
        let read_back2: Option<Vec<String>> = read(&path).unwrap();
        assert_eq!(read_back2, Some(value2));
        let leftover: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftover.is_empty(), "temp files left behind: {leftover:?}");
    }

    #[test]
    fn write_atomic_creates_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("nested").join("data");
        let path = nested.join("index.json");
        write_atomic(&nested, &path, &"value".to_string()).unwrap();
        let read_back: Option<String> = read(&path).unwrap();
        assert_eq!(read_back, Some("value".to_string()));
    }

    #[test]
    fn read_versioned_round_trips_through_the_envelope() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq, Debug)]
        struct Payload {
            items: Vec<String>,
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.json");
        let payload = Payload {
            items: vec!["x".to_string()],
        };
        write_versioned(dir.path(), &path, &payload).unwrap();

        // The envelope carries the format version alongside the flattened payload.
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"version\": 1"));
        assert!(text.contains("\"items\""));

        let back: Option<Payload> = read_versioned(&path).unwrap();
        assert_eq!(back, Some(payload));
        // A missing versioned file reads as `None`.
        let missing: Option<Payload> = read_versioned(&dir.path().join("nope.json")).unwrap();
        assert_eq!(missing, None);
    }

    #[test]
    fn fsync_parent_dir_succeeds_for_a_real_directory() {
        // The directory exists and opens, so the best-effort sync runs without
        // panicking and the function returns normally.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        fs::write(&path, "x").unwrap();
        fsync_parent_dir(&path);
    }

    #[test]
    fn fsync_parent_dir_swallows_an_unopenable_parent() {
        // `Path::new("bare.md").parent()` is `Some("")`, and opening "" fails;
        // the error is swallowed rather than propagated, so this must not panic.
        fsync_parent_dir(Path::new("bare.md"));
    }

    #[test]
    fn fsync_parent_dir_is_a_noop_without_a_parent() {
        // `Path::new("").parent()` is `None`, exercising the early return.
        fsync_parent_dir(Path::new(""));
    }

    #[test]
    fn unique_tmp_path_differs_per_call_and_keeps_target_name() {
        let path = Path::new("/data/index.json");
        let a = unique_tmp_path(path);
        let b = unique_tmp_path(path);
        assert_ne!(a, b, "two calls must yield distinct temp names");
        for tmp in [&a, &b] {
            let name = tmp.file_name().unwrap().to_string_lossy();
            assert!(name.starts_with("index.json.tmp."), "unexpected: {name}");
        }
    }
}

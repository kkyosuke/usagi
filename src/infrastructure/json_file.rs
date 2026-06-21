//! Shared helpers for the files usagi persists under its data directories.
//!
//! Every store treats a missing file as "no data yet" and writes through a temp
//! file + rename so a crash never leaves a half-written file behind. JSON stores
//! (`storage`, `workspace_store`, `history_store`) use [`read`] / [`write_atomic`];
//! the markdown stores (`issue_store`, `memory_store`) use [`write_text_atomic`]
//! for their hand-rolled text. All share one per-writer-unique temp-name scheme
//! so two processes writing the same path never clobber each other.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Serialize};

/// Bumped per atomic write so each write within this process gets a distinct
/// temp file name; combined with the pid it is unique across processes too.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A per-writer-unique temp path for `path`: the full target name with a
/// `.tmp.<pid>.<counter>` suffix appended.
///
/// A fixed `*.tmp` would let two processes writing the same path (e.g. the MCP
/// server and the TUI both editing one repo's `index.json`, or agent-phase
/// hooks firing concurrently for one worktree) clobber each other's
/// half-written temp before the rename. The pid keeps the name unique across
/// processes, the counter within one.
fn unique_tmp_path(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(
        ".tmp.{}.{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    PathBuf::from(tmp)
}

/// Read and deserialize the JSON file at `path`, returning `None` if it does
/// not exist.
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

/// Serialize `value` to pretty JSON and write it atomically to `path` (temp
/// file + rename), creating `dir` (the directory that contains `path`) first.
pub fn write_atomic<T: Serialize>(dir: &Path, path: &Path, value: &T) -> Result<()> {
    fs::create_dir_all(dir).context(format!("failed to create {}", dir.display()))?;
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    let tmp = unique_tmp_path(path);
    fs::write(&tmp, text).context(format!("failed to write {}", tmp.display()))?;
    fs::rename(&tmp, path).context(format!("failed to replace {}", path.display()))?;
    Ok(())
}

/// Write `text` to `path` atomically (per-writer-unique temp file + rename) so
/// a crash never leaves a half-written file behind and two processes writing
/// the same path never clobber each other's temp. Unlike [`write_atomic`],
/// `text` is written verbatim and the parent directory is assumed to exist
/// already — the markdown stores create it when they set up their data dir.
pub fn write_text_atomic(path: &Path, text: &str) -> Result<()> {
    let tmp = unique_tmp_path(path);
    fs::write(&tmp, text).context(format!("failed to write {}", tmp.display()))?;
    fs::rename(&tmp, path).context(format!("failed to replace {}", path.display()))?;
    Ok(())
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

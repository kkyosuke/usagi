//! Shared helpers for the JSON files usagi persists under its data directories.
//!
//! Every store (`storage`, `workspace_store`, `history_store`) treats a missing
//! file as "no data yet" and writes through a temp file + rename so a crash
//! never leaves a half-written file behind. These two helpers capture that
//! shared behaviour so each store only has to describe its on-disk shape.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Serialize};

/// Bumped per `write_atomic` call so each write within this process gets a
/// distinct temp file name; combined with the pid it is unique across processes
/// too.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    // Give the temp file a per-writer-unique name: a fixed `*.json.tmp` would let
    // two processes writing the same path (e.g. agent-phase hooks firing
    // concurrently for one worktree) clobber each other's half-written temp
    // before the rename. The pid keeps it unique across processes, the counter
    // within one.
    let suffix = format!(
        "json.tmp.{}.{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let tmp = path.with_extension(suffix);
    fs::write(&tmp, text).context(format!("failed to write {}", tmp.display()))?;
    fs::rename(&tmp, path).context(format!("failed to replace {}", path.display()))?;
    Ok(())
}

//! Per-worktree queue of prompts to inject into a session's *already running*
//! agent, drained live by the TUI while it runs.
//!
//! This is the live counterpart of [`super::agent_prompt_store`]. Both are fed by
//! the MCP `session_prompt` tool, which routes to one or the other by its `mode`:
//! where that store hands a prompt to the agent on its *next fresh launch* (the
//! launch channel), this one delivers to an agent pane that is *already open* (the
//! live channel). `session_prompt` appends a prompt here, and the running home
//! screen's terminal-pool watcher drains it every tick and types it — followed by
//! a submit — straight into that session's live agent pane (see
//! [`crate::presentation::tui::home::terminal::pool`]). Nothing else can reach a
//! running pane across the process boundary: the MCP server runs in the separate
//! `usagi mcp` process, which shares no memory with the TUI, so the two agree on
//! a file path purely from the worktree directory.
//!
//! Unlike the one-shot launch prompt, several sends can pile up before the
//! watcher next drains them. The current on-disk format stores one directory per
//! worktree: `meta.json` plus one item file per active queued prompt. Bounded
//! fresh-spawn failures move their batch into the metadata's explicit
//! `dead_batches` array, so a later append remains active while a live pane can
//! recover dead work oldest-first. The item layout keeps ordinary append I/O
//! bounded to the new item plus metadata instead of rewriting every queued
//! prompt on every send. Older single-file `Vec<String>` queues and pre-split
//! dead retry metadata are migrated lazily under the same store lock.
//!
//! Like [`super::agent_prompt_store`], the file is addressed purely from the
//! worktree directory: its canonical form hashed to a stable, filesystem-safe
//! name under `<data-dir>/agent-live-prompts/` (the addressing is shared via
//! [`crate::infrastructure::worktree_keyed_store`]). Each file also records the
//! worktree it belongs to, so a hashed-name collision (or a stale file synced
//! from another machine) is detected and read as absent rather than
//! misattributed.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::infrastructure::agent_prompt_store::{retry_state_after_failure, RetryState};
use crate::infrastructure::json_file;
use crate::infrastructure::store_lock::{self, StoreLock};
use crate::infrastructure::worktree_keyed_store::{
    dir, file_name, key, path_for, read_ours, WorktreeStamped,
};

/// Subdirectory of the data dir the live-prompt files live under.
const PROMPT_SUBDIR: &str = "agent-live-prompts";
const META_FILE: &str = "meta.json";

/// Maximum live prompts queued for one worktree.
pub const MAX_LIVE_QUEUE_ITEMS: usize = 64;
/// Maximum UTF-8 payload bytes queued for one worktree.
pub const MAX_LIVE_QUEUE_BYTES: usize = 512 * 1024;
/// Maximum prompts drained for one delivery pass.
pub const MAX_LIVE_BATCH_ITEMS: usize = 4;
/// Maximum UTF-8 payload bytes drained for one delivery pass.
pub const MAX_LIVE_BATCH_BYTES: usize = 256 * 1024;

/// On-disk shape of a worktree's live-prompt queue.
#[derive(Serialize, Deserialize)]
struct LivePromptFile {
    /// The worktree this queue belongs to. Stored so a hashed-name collision is
    /// caught: a read whose recorded worktree differs is treated as absent.
    worktree: PathBuf,
    /// The prompts awaiting delivery to the session's live agent, in send order.
    prompts: Vec<String>,
}

impl WorktreeStamped for LivePromptFile {
    fn stamped(&self) -> &Path {
        &self.worktree
    }
}

#[derive(Serialize, Deserialize)]
struct LivePromptMeta {
    worktree: PathBuf,
    next_id: u64,
    /// Total prompt count across active item files and `dead_batches`.
    count: usize,
    /// Total UTF-8 payload bytes across active item files and `dead_batches`.
    bytes: usize,
    /// Retry state for a pane-less background spawn. Missing on older files.
    /// A live agent deliberately bypasses and clears this state.
    #[serde(default)]
    retry: Option<RetryState>,
    /// Number of leading active item files governed by `retry`. Appends during
    /// backoff sit beyond this boundary and never inherit the older failure.
    #[serde(default)]
    retry_count: usize,
    /// Failed batches retired from the active fresh-spawn queue. A later active
    /// append must remain independently spawnable, while a live pane can still
    /// recover these oldest-first.
    #[serde(default)]
    dead_batches: Vec<DeadLivePromptBatch>,
}

impl WorktreeStamped for LivePromptMeta {
    fn stamped(&self) -> &Path {
        &self.worktree
    }
}

#[derive(Serialize, Deserialize)]
struct LivePromptItem {
    prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DeadLivePromptBatch {
    prompts: Vec<String>,
    retry: RetryState,
}

/// A bounded live-prompt batch removed for an in-flight fresh-agent spawn.
///
/// The retry state travels with the claimed prompts so cancellation can restore
/// it without counting a failure, even when newer prompts arrive meanwhile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakenLivePromptBatch {
    pub prompts: Vec<String>,
    pub retry: Option<RetryState>,
}

/// Strict result of claiming pane-less live work for a fresh-agent spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TakeReadyForSpawn {
    Ready(TakenLivePromptBatch),
    Waiting(RetryState),
    Dead(RetryState),
    Empty,
}

fn queue_dir(root: &Path, key: &Path) -> PathBuf {
    root.join(file_name(key))
}

fn meta_path(queue: &Path) -> PathBuf {
    queue.join(META_FILE)
}

fn item_name(id: u64) -> String {
    format!("{id:020}.json")
}

fn item_id(path: &Path) -> Option<u64> {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".json"))
        .and_then(|name| (name != "meta").then_some(name))
        .and_then(|name| name.parse().ok())
}

fn read_item(path: &Path) -> Result<String> {
    let item: LivePromptItem =
        json_file::read(path)?.context(format!("missing live prompt item {}", path.display()))?;
    Ok(item.prompt)
}

fn item_paths(queue: &Path) -> Vec<(u64, PathBuf)> {
    let mut paths: Vec<(u64, PathBuf)> = fs::read_dir(queue)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            item_id(&path).map(|id| (id, path))
        })
        .collect();
    paths.sort_by_key(|(id, _)| *id);
    paths
}

fn write_meta(queue: &Path, meta: &LivePromptMeta) -> Result<()> {
    json_file::write_atomic(queue, &meta_path(queue), meta)
}

fn read_meta(queue: &Path, key: &Path) -> Option<LivePromptMeta> {
    read_ours::<LivePromptMeta>(&meta_path(queue), key)
}

fn unix_secs(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

/// Load and validate the complete item set while the caller holds the store
/// lock. Spawn decisions fail closed on malformed state instead of treating an
/// unreadable durable queue as empty.
fn load_items_strict(queue: &Path, meta: &LivePromptMeta) -> Result<Vec<(PathBuf, String)>> {
    let mut dead_count = 0usize;
    let mut dead_bytes = 0usize;
    for batch in &meta.dead_batches {
        if batch.prompts.is_empty() || !batch.retry.dead {
            bail!(
                "live prompt queue {} has an invalid dead batch",
                queue.display()
            );
        }
        dead_count = dead_count
            .checked_add(batch.prompts.len())
            .context("live prompt dead-batch item count overflow")?;
        for prompt in &batch.prompts {
            dead_bytes = dead_bytes
                .checked_add(prompt.len())
                .context("live prompt dead-batch byte count overflow")?;
        }
    }
    let active_count = meta.count.checked_sub(dead_count).ok_or_else(|| {
        anyhow!(
            "live prompt queue {} records fewer total items than dead items",
            queue.display()
        )
    })?;
    let active_bytes = meta.bytes.checked_sub(dead_bytes).ok_or_else(|| {
        anyhow!(
            "live prompt queue {} records fewer total bytes than dead bytes",
            queue.display()
        )
    })?;
    if meta.retry.is_none() && meta.retry_count != 0 {
        bail!(
            "live prompt queue {} has retry_count {} without retry state",
            queue.display(),
            meta.retry_count
        );
    }
    if meta.retry.is_some() && active_count == 0 {
        bail!(
            "live prompt queue {} has retry state without active items",
            queue.display()
        );
    }
    if meta.retry_count > active_count {
        bail!(
            "live prompt queue {} has retry_count {} but only {active_count} active items",
            queue.display(),
            meta.retry_count
        );
    }
    let paths = item_paths(queue);
    if paths.len() != active_count {
        bail!(
            "live prompt queue {} has {} active item files but metadata records {active_count}",
            queue.display(),
            paths.len()
        );
    }
    let mut items = Vec::with_capacity(paths.len());
    let mut bytes = 0usize;
    for (_, path) in paths {
        let prompt = read_item(&path)?;
        bytes = bytes.saturating_add(prompt.len());
        items.push((path, prompt));
    }
    if bytes != active_bytes {
        bail!(
            "live prompt queue {} has {bytes} active payload bytes but metadata records {active_bytes}",
            queue.display(),
        );
    }
    Ok(items)
}

fn bounded_batch(items: &[(PathBuf, String)]) -> (Vec<String>, Vec<PathBuf>, usize) {
    let mut prompts = Vec::new();
    let mut paths = Vec::new();
    let mut bytes = 0usize;
    for (path, prompt) in items {
        let prompt_bytes = prompt.len();
        if !prompts.is_empty()
            && (prompts.len() >= MAX_LIVE_BATCH_ITEMS
                || bytes.saturating_add(prompt_bytes) > MAX_LIVE_BATCH_BYTES)
        {
            break;
        }
        bytes = bytes.saturating_add(prompt_bytes);
        prompts.push(prompt.clone());
        paths.push(path.clone());
        if prompts.len() >= MAX_LIVE_BATCH_ITEMS || bytes >= MAX_LIVE_BATCH_BYTES {
            break;
        }
    }
    (prompts, paths, bytes)
}

/// Normalize retry metadata written before `retry_count` and dead-batch
/// separation. The old retry governed at most one bounded delivery batch, so a
/// missing boundary becomes the same bounded active prefix rather than every
/// prompt appended while it was backing off.
fn normalize_retry_state_locked(
    root: &Path,
    key: &Path,
    mut meta: LivePromptMeta,
    items: Vec<(PathBuf, String)>,
) -> Result<(LivePromptMeta, Vec<(PathBuf, String)>)> {
    let queue = queue_dir(root, key);
    if meta.retry.is_none() {
        return Ok((meta, items));
    }
    if meta.retry_count == 0 {
        let (_, paths, _) = bounded_batch(&items);
        meta.retry_count = paths.len();
        write_meta(&queue, &meta)?;
    }
    if !meta.retry.as_ref().is_some_and(|retry| retry.dead) {
        return Ok((meta, items));
    }
    let retry = meta.retry.take().expect("dead retry checked above");
    let retry_count = meta.retry_count;
    meta.retry_count = 0;
    let mut prompts = items
        .into_iter()
        .map(|(_, prompt)| prompt)
        .collect::<Vec<_>>();
    let active = prompts.split_off(retry_count);
    meta.dead_batches
        .push(DeadLivePromptBatch { prompts, retry });
    let normalized = rewrite_queue_locked(root, key, active, None, 0, meta.dead_batches)?;
    let items = load_items_strict(&queue, &normalized)?;
    Ok((normalized, items))
}

fn migrate_legacy_locked(root: &Path, key: &Path) -> Result<()> {
    let legacy = root.join(file_name(key));
    if legacy.is_dir() {
        return Ok(());
    }
    let Some(file) = read_ours::<LivePromptFile>(&legacy, key) else {
        return Ok(());
    };
    let queue = queue_dir(root, key);
    let _ = fs::remove_file(&legacy);
    fs::create_dir_all(&queue).context(format!("failed to create {}", queue.display()))?;
    let mut meta = LivePromptMeta {
        worktree: key.to_path_buf(),
        next_id: 0,
        count: 0,
        bytes: 0,
        retry: None,
        retry_count: 0,
        dead_batches: Vec::new(),
    };
    for prompt in file.prompts {
        if meta.count >= MAX_LIVE_QUEUE_ITEMS
            || meta.bytes.saturating_add(prompt.len()) > MAX_LIVE_QUEUE_BYTES
        {
            break;
        }
        let id = meta.next_id;
        meta.next_id += 1;
        meta.count += 1;
        meta.bytes += prompt.len();
        let path = queue.join(item_name(id));
        let item = LivePromptItem { prompt };
        json_file::write_atomic(&queue, &path, &item)?;
    }
    write_meta(&queue, &meta)?;
    Ok(())
}

fn rewrite_queue_locked(
    root: &Path,
    key: &Path,
    prompts: Vec<String>,
    retry: Option<RetryState>,
    retry_count: usize,
    dead_batches: Vec<DeadLivePromptBatch>,
) -> Result<LivePromptMeta> {
    if retry.is_none() && retry_count != 0 {
        bail!("cannot persist live retry_count without retry state");
    }
    if retry.is_some() && (retry_count == 0 || retry_count > prompts.len()) {
        bail!(
            "cannot persist live retry_count {retry_count} for {} active prompts",
            prompts.len()
        );
    }
    let queue = queue_dir(root, key);
    if queue.is_file() {
        let _ = fs::remove_file(&queue);
    }
    if queue.exists() {
        let _ = fs::remove_dir_all(&queue);
    }
    fs::create_dir_all(&queue).context(format!("failed to create {}", queue.display()))?;
    let dead_count = dead_batches
        .iter()
        .map(|batch| batch.prompts.len())
        .sum::<usize>();
    let dead_bytes = dead_batches
        .iter()
        .flat_map(|batch| &batch.prompts)
        .map(|prompt| prompt.len())
        .sum::<usize>();
    let mut meta = LivePromptMeta {
        worktree: key.to_path_buf(),
        next_id: 0,
        count: dead_count,
        bytes: dead_bytes,
        retry,
        retry_count,
        dead_batches,
    };
    for prompt in prompts {
        let id = meta.next_id;
        meta.next_id += 1;
        meta.count += 1;
        meta.bytes += prompt.len();
        let path = queue.join(item_name(id));
        let item = LivePromptItem { prompt };
        json_file::write_atomic(&queue, &path, &item)?;
    }
    write_meta(&queue, &meta)?;
    Ok(meta)
}

/// Append `prompt` to the live queue for the session rooted at `worktree`, to be
/// delivered to its already-running agent pane. Preserves any prompts already
/// queued (unlike [`super::agent_prompt_store::set`], which replaces), so several
/// sends before the watcher next drains are all delivered in order.
pub fn append(worktree: &Path, prompt: &str) -> Result<()> {
    let key = key(worktree);
    let dir = dir(PROMPT_SUBDIR)?;
    // Hold the store lock across the read-modify-write so a concurrent `append`
    // (another live `session_prompt`) or a `take_all` (the TUI draining) cannot
    // race and drop a queued prompt.
    let _lock = StoreLock::acquire(&dir)?;
    migrate_legacy_locked(&dir, &key)?;
    let queue = queue_dir(&dir, &key);
    if queue.is_file() {
        let _ = fs::remove_file(&queue);
    }
    fs::create_dir_all(&queue).context(format!("failed to create {}", queue.display()))?;
    let mut meta = match read_meta(&queue, &key) {
        Some(meta) => {
            let items = load_items_strict(&queue, &meta)?;
            normalize_retry_state_locked(&dir, &key, meta, items)?.0
        }
        None => LivePromptMeta {
            worktree: key.clone(),
            next_id: 0,
            count: 0,
            bytes: 0,
            retry: None,
            retry_count: 0,
            dead_batches: Vec::new(),
        },
    };
    if meta.count >= MAX_LIVE_QUEUE_ITEMS {
        bail!(
            "live prompt queue is full ({} items; limit is {MAX_LIVE_QUEUE_ITEMS})",
            meta.count
        );
    }
    let next_bytes = meta.bytes.saturating_add(prompt.len());
    if next_bytes > MAX_LIVE_QUEUE_BYTES {
        bail!(
            "live prompt queue is full ({} bytes plus {} bytes; limit is {MAX_LIVE_QUEUE_BYTES})",
            meta.bytes,
            prompt.len()
        );
    }
    let id = meta.next_id;
    meta.next_id += 1;
    meta.count += 1;
    meta.bytes = next_bytes;
    let path = queue.join(item_name(id));
    let item = LivePromptItem {
        prompt: prompt.to_string(),
    };
    json_file::write_atomic(&queue, &path, &item)?;
    write_meta(&queue, &meta)
}

/// Take (read and remove) every prompt queued for the session rooted at
/// `worktree`, in send order, or an empty vector when none is queued (or the
/// file belongs to a different worktree). Removing the file makes delivery
/// one-shot: a later drain that finds nothing returns empty.
///
/// The common case each watcher tick is an empty queue, so this first checks the
/// file's existence cheaply and returns without taking the store lock when there
/// is nothing to drain.
pub fn take_all(worktree: &Path) -> Vec<String> {
    take_batch(worktree)
}

/// Whether this worktree has live-channel work, validated under the queue lock.
/// Used before destructive exited-pane retirement so a queue in some *other*
/// session can never remove this session's bare shell.
pub fn has_queued(worktree: &Path) -> Result<bool> {
    let key = key(worktree);
    let dir = dir(PROMPT_SUBDIR)?;
    let _lock = StoreLock::acquire(&dir)?;
    migrate_legacy_locked(&dir, &key)?;
    let queue = queue_dir(&dir, &key);
    if !queue.exists() {
        return Ok(false);
    }
    let meta = read_meta(&queue, &key).ok_or_else(|| {
        anyhow!(
            "live prompt metadata {} is unreadable",
            meta_path(&queue).display()
        )
    })?;
    load_items_strict(&queue, &meta)
        .map(|_| meta.count > 0)
        .map_err(|error| {
            anyhow!(
                "live prompt queue {} is inconsistent: {error:#}",
                queue.display()
            )
        })
}

/// Take one bounded delivery batch from the live queue.
pub fn take_batch(worktree: &Path) -> Vec<String> {
    // Fast path: no file means nothing queued. Skips the lock (and its own lock
    // file creation) on the overwhelmingly common empty tick.
    match path_for(PROMPT_SUBDIR, worktree) {
        Ok(path) if path.exists() => {}
        _ => return Vec::new(),
    }
    // The file existed a moment ago; drain it under the lock. A missing data dir
    // or a contended lock yields nothing, leaving anything queued for a later tick.
    drain_batch(worktree).unwrap_or_default()
}

/// Strictly claim one bounded batch for a pane-less background fresh spawn.
///
/// Waiting and dead-letter records remain durable. An active ready batch takes
/// precedence over older separated dead batches; `Dead` is returned only when
/// no active item exists. Only a ready batch is removed, and its retry state is
/// returned with it for cancellation/failure handling. Store, lock, metadata,
/// and item errors are surfaced so an unattended caller cannot mistake
/// uncertain ownership for an empty queue.
pub fn take_ready_for_spawn(worktree: &Path, now: SystemTime) -> Result<TakeReadyForSpawn> {
    let key = key(worktree);
    let dir = dir(PROMPT_SUBDIR)?;
    let _lock = StoreLock::acquire(&dir)?;
    migrate_legacy_locked(&dir, &key)?;
    let queue = queue_dir(&dir, &key);
    if !queue.exists() {
        return Ok(TakeReadyForSpawn::Empty);
    }
    let meta = read_meta(&queue, &key).ok_or_else(|| {
        anyhow!(
            "live prompt metadata {} is unreadable or belongs to another worktree",
            meta_path(&queue).display()
        )
    })?;
    let items = load_items_strict(&queue, &meta)?;
    let (mut meta, items) = normalize_retry_state_locked(&dir, &key, meta, items)?;
    if items.is_empty() {
        return Ok(meta
            .dead_batches
            .first()
            .map(|batch| TakeReadyForSpawn::Dead(batch.retry.clone()))
            .unwrap_or(TakeReadyForSpawn::Empty));
    }
    if let Some(retry) = meta
        .retry
        .as_ref()
        .filter(|retry| retry.next_retry_unix_secs > unix_secs(now))
    {
        return Ok(TakeReadyForSpawn::Waiting(retry.clone()));
    }

    let eligible = if meta.retry.is_some() {
        &items[..meta.retry_count]
    } else {
        &items[..]
    };
    let (prompts, paths, bytes) = bounded_batch(eligible);
    for path in &paths {
        let context = format!("failed to remove claimed live prompt {}", path.display());
        fs::remove_file(path).context(context)?;
    }
    let taken = TakenLivePromptBatch {
        prompts,
        retry: meta.retry.take(),
    };
    meta.count = meta.count.saturating_sub(paths.len());
    meta.bytes = meta.bytes.saturating_sub(bytes);
    // Retry belongs to the claimed oldest batch. Newer queued work starts clean.
    meta.retry = None;
    meta.retry_count = 0;
    if meta.count == 0 {
        let context = format!("failed to remove empty live queue {}", queue.display());
        fs::remove_dir_all(&queue).context(context)?;
    } else {
        write_meta(&queue, &meta)?;
    }
    Ok(TakeReadyForSpawn::Ready(taken))
}

/// Read-and-remove the queued prompts under the store lock, or `None` when the
/// data dir or lock is unavailable. Split from [`take_all`] so those two early
/// exits collapse onto single `?` lines rather than never-taken `return` arms.
fn drain_batch(worktree: &Path) -> Option<Vec<String>> {
    let key = key(worktree);
    let dir = dir(PROMPT_SUBDIR).ok()?;
    // Serialise the read-then-remove against `append` (see there): without the
    // lock an `append` landing between the read below and the remove would have
    // its file deleted and its prompt never delivered. If the lock cannot be
    // taken, leave everything queued for a later drain rather than risk loss.
    let _lock = StoreLock::acquire(&dir).ok()?;
    migrate_legacy_locked(&dir, &key).ok()?;
    let queue = queue_dir(&dir, &key);
    let meta = read_meta(&queue, &key)?;
    let items = load_items_strict(&queue, &meta).ok()?;
    let (mut meta, items) = normalize_retry_state_locked(&dir, &key, meta, items).ok()?;

    // Once a live pane is available, promote every dead batch back into the
    // ordinary oldest-first stream. Rewriting the unclaimed tail as active work
    // is important: if delivery of the returned prefix fails, the existing
    // `requeue` API can put it back ahead of this tail without reversing it with
    // a still-separate dead queue.
    if !meta.dead_batches.is_empty() {
        let mut combined = meta
            .dead_batches
            .into_iter()
            .flat_map(|batch| batch.prompts)
            .collect::<Vec<_>>();
        combined.extend(items.into_iter().map(|(_, prompt)| prompt));
        let indexed = combined
            .iter()
            .enumerate()
            .map(|(index, prompt)| (PathBuf::from(index.to_string()), prompt.clone()))
            .collect::<Vec<_>>();
        let (batch, _, _) = bounded_batch(&indexed);
        let remaining = combined.split_off(batch.len());
        if remaining.is_empty() {
            fs::remove_dir_all(&queue).ok()?;
        } else {
            rewrite_queue_locked(&dir, &key, remaining, None, 0, Vec::new()).ok()?;
        }
        return Some(batch);
    }

    let mut batch = Vec::new();
    let mut batch_bytes = 0usize;
    let mut taken = Vec::new();
    for (path, prompt) in items {
        let prompt_bytes = prompt.len();
        if !batch.is_empty()
            && (batch.len() >= MAX_LIVE_BATCH_ITEMS
                || batch_bytes.saturating_add(prompt_bytes) > MAX_LIVE_BATCH_BYTES)
        {
            break;
        }
        batch_bytes = batch_bytes.saturating_add(prompt_bytes);
        batch.push(prompt);
        taken.push((path, prompt_bytes));
        if batch.len() >= MAX_LIVE_BATCH_ITEMS || batch_bytes >= MAX_LIVE_BATCH_BYTES {
            break;
        }
    }
    for (path, _) in &taken {
        let _ = fs::remove_file(path);
    }
    meta.count = meta.count.saturating_sub(taken.len());
    meta.bytes = meta.bytes.saturating_sub(batch_bytes);
    // A live pane is a healthy delivery path: background-spawn backoff and even
    // dead-letter state must not prevent it from draining this queue.
    meta.retry = None;
    meta.retry_count = 0;
    if meta.count == 0 {
        let _ = fs::remove_dir_all(&queue);
    } else {
        write_meta(&queue, &meta).ok()?;
    }
    Some(batch)
}

/// Put `prompts` back at the **front** of the live queue for the session rooted
/// at `worktree`, ahead of anything queued since they were taken.
///
/// [`take_all`] drains the whole queue in one shot before the caller delivers
/// it; if delivery then fails partway (e.g. a PTY write to a wedged pane), the
/// undelivered tail would otherwise be lost even though the caller was told the
/// prompts were queued. This returns those prompts to the store in their
/// original order so a later tick retries them. Because a concurrent [`append`]
/// may have landed between the drain and here, the returned prompts are placed
/// *before* the newer ones, preserving overall send order. An empty slice is a
/// no-op (the file is left untouched). Restoration may temporarily exceed the
/// public append limits when producers filled the capacity after the take. That
/// is intentional: every prompt was already accepted, so dropping the in-flight
/// prefix would be worse. Subsequent [`append`] calls remain rejected until
/// delivery brings the total back below both limits. Store/lock/write failures
/// are returned to the caller; this function never silently reports a restore.
pub fn requeue(worktree: &Path, prompts: &[String]) -> Result<()> {
    if prompts.is_empty() {
        return Ok(());
    }
    let key = key(worktree);
    let dir = dir(PROMPT_SUBDIR)?;
    // Hold the store lock across the read-modify-write for the same reason
    // [`append`] does: a concurrent append/drain must not race and drop a prompt.
    let _lock = StoreLock::acquire(&dir)?;
    migrate_legacy_locked(&dir, &key)?;
    let queue = queue_dir(&dir, &key);
    let existing = if queue.is_file() {
        // A legacy-path collision stamped for another worktree is not ours.
        // Rewriting below removes that file and starts a queue stamped as ours,
        // matching append's established collision recovery behavior.
        Vec::new()
    } else if queue.exists() {
        let meta = read_meta(&queue, &key).ok_or_else(|| {
            anyhow!(
                "live prompt metadata {} is unreadable or belongs to another worktree",
                meta_path(&queue).display()
            )
        })?;
        let items = load_items_strict(&queue, &meta)?;
        let (meta, items) = normalize_retry_state_locked(&dir, &key, meta, items)?;
        meta.dead_batches
            .into_iter()
            .flat_map(|batch| batch.prompts)
            .chain(items.into_iter().map(|(_, prompt)| prompt))
            .collect()
    } else {
        Vec::new()
    };
    let mut merged = prompts.to_vec();
    merged.extend(existing);
    // A live-pane delivery bypasses background spawn backoff. If it has to put
    // work back, the healthy live channel remains immediately eligible.
    rewrite_queue_locked(&dir, &key, merged, None, 0, Vec::new()).map(|_| ())
}

fn requeue_spawn_batch_with_state(
    worktree: &Path,
    taken: &TakenLivePromptBatch,
    retry: Option<RetryState>,
) -> Result<()> {
    if taken.prompts.is_empty() {
        return Ok(());
    }
    let key = key(worktree);
    let dir = dir(PROMPT_SUBDIR)?;
    let _lock = StoreLock::acquire(&dir)?;
    migrate_legacy_locked(&dir, &key)?;
    let queue = queue_dir(&dir, &key);
    let (existing, dead_batches) = if queue.exists() {
        let meta = read_meta(&queue, &key).ok_or_else(|| {
            anyhow!(
                "live prompt metadata {} is unreadable or belongs to another worktree",
                meta_path(&queue).display()
            )
        })?;
        let items = load_items_strict(&queue, &meta)?;
        let (meta, items) = normalize_retry_state_locked(&dir, &key, meta, items)?;
        (
            items.into_iter().map(|(_, prompt)| prompt).collect(),
            meta.dead_batches,
        )
    } else {
        (Vec::new(), Vec::new())
    };
    let mut merged = taken.prompts.clone();
    merged.extend(existing);
    // The batch was accepted before it was claimed. Concurrent producers may
    // have refilled the advertised capacity, but restoration must remain
    // lossless; append applies the total limits again after this rewrite.
    let retry_count = retry.as_ref().map_or(0, |_| taken.prompts.len());
    rewrite_queue_locked(&dir, &key, merged, retry, retry_count, dead_batches).map(|_| ())
}

fn retire_spawn_batch_as_dead(
    worktree: &Path,
    taken: &TakenLivePromptBatch,
    retry: RetryState,
) -> Result<()> {
    let key = key(worktree);
    let dir = dir(PROMPT_SUBDIR)?;
    let _lock = StoreLock::acquire(&dir)?;
    migrate_legacy_locked(&dir, &key)?;
    let queue = queue_dir(&dir, &key);
    let (active, active_retry, active_retry_count, mut dead_batches) = if queue.exists() {
        let meta = read_meta(&queue, &key).ok_or_else(|| {
            anyhow!(
                "live prompt metadata {} is unreadable or belongs to another worktree",
                meta_path(&queue).display()
            )
        })?;
        let items = load_items_strict(&queue, &meta)?;
        let (meta, items) = normalize_retry_state_locked(&dir, &key, meta, items)?;
        (
            items.into_iter().map(|(_, prompt)| prompt).collect(),
            meta.retry,
            meta.retry_count,
            meta.dead_batches,
        )
    } else {
        (Vec::new(), None, 0, Vec::new())
    };
    if !taken.prompts.is_empty() {
        dead_batches.push(DeadLivePromptBatch {
            prompts: taken.prompts.clone(),
            retry,
        });
    }
    rewrite_queue_locked(
        &dir,
        &key,
        active,
        active_retry,
        active_retry_count,
        dead_batches,
    )
    .map(|_| ())
}

/// Restore a spawn claim at the front without counting a new failure.
///
/// The claimed retry state is preserved and newer concurrently appended work
/// remains behind it in send order.
pub fn requeue_taken(worktree: &Path, taken: &TakenLivePromptBatch) -> Result<()> {
    requeue_spawn_batch_with_state(worktree, taken, taken.retry.clone())
}

/// Restore a failed fresh-spawn claim with incremented durable retry state.
///
/// Uses the launch-prompt store's shared bounded exponential-backoff and
/// dead-letter policy so both unattended spawn channels escalate consistently.
pub fn requeue_after_spawn_failure(
    worktree: &Path,
    taken: &TakenLivePromptBatch,
    error: &str,
    now: SystemTime,
) -> Result<RetryState> {
    let retry = retry_state_after_failure(taken.retry.as_ref(), error, now);
    if retry.dead {
        retire_spawn_batch_as_dead(worktree, taken, retry.clone())?;
    } else {
        requeue_spawn_batch_with_state(worktree, taken, Some(retry.clone()))?;
    }
    Ok(retry)
}

/// Discard any prompts queued for `worktree` (best-effort), so a session removed
/// before its agent drained them — and later recreated at the same path — does
/// not inherit prompts sent to the previous session. Called from session removal
/// (see [`crate::usecase::session::remove`]); a no-op when nothing is queued.
pub fn clear(worktree: &Path) {
    let _ = try_clear(worktree);
}

pub fn try_clear(worktree: &Path) -> Result<()> {
    let path = path_for(PROMPT_SUBDIR, worktree)?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path).map_err(Into::into),
        Ok(_) => fs::remove_file(path).map_err(Into::into),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Whether any prompt is currently queued for *some* worktree's live agent — a
/// cheap directory listing, so the home screen's autostart pass can skip the
/// per-session lookup on the common tick where nothing is queued.
///
/// The mirror of [`super::agent_prompt_store::any_queued`] for the live channel:
/// autostart consults both, since a pane-less session may have a prompt stranded
/// in *either* store. `session_prompt`'s explicit `mode="live"` always appends here
/// without checking for a live pane, so a prompt sent that way to a session with no
/// live agent pane waits here until one opens. (`auto` no longer strands prompts
/// here — it routes live only when the pid-stamped live-pane marker confirms a
/// running consumer, see [`super::agent_live_pane_store`].) Deliberately coarse —
/// it does not read or validate the files (a
/// [`take_all`] still confirms each queue belongs to the worktree it is keyed
/// under); it only reports whether the directory holds anything besides the
/// shared [`StoreLock`] file. A missing directory reads as empty.
pub fn any_queued() -> bool {
    dir(PROMPT_SUBDIR)
        .ok()
        .and_then(|dir| fs::read_dir(dir).ok())
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            // The store lock lives alongside the prompt files; it is not a queued
            // prompt, so a directory holding only the lock reads as empty.
            entry.file_type().is_ok_and(|kind| kind.is_dir())
                || (entry.file_type().is_ok_and(|kind| kind.is_file())
                    && entry.file_name().to_str() != Some(store_lock::LOCK_FILE_NAME))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::agent_prompt_store::MAX_PROMPT_RETRY_ATTEMPTS;
    use crate::infrastructure::json_file;
    use crate::infrastructure::storage;
    use std::sync::{Arc, Barrier};
    use std::thread;

    /// Point `$USAGI_HOME` at a throwaway directory for the duration of a test,
    /// serialized against other env-mutating tests, and run `body` with it.
    fn with_data_dir(body: impl FnOnce(&Path)) {
        let _guard = crate::test_support::process_env_guard();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(storage::DATA_DIR_ENV, dir.path());
        body(dir.path());
        std::env::remove_var(storage::DATA_DIR_ENV);
    }

    fn ready_batch(result: TakeReadyForSpawn) -> TakenLivePromptBatch {
        match result {
            TakeReadyForSpawn::Ready(taken) => taken,
            other => panic!("expected ready live-prompt batch, got {other:?}"),
        }
    }

    #[test]
    fn append_then_take_all_round_trips_in_order_and_is_one_shot() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            // Nothing queued yet.
            assert!(take_all(wt.path()).is_empty());
            // Several sends pile up and are drained together, in send order.
            append(wt.path(), "first").unwrap();
            append(wt.path(), "second").unwrap();
            assert_eq!(take_all(wt.path()), vec!["first", "second"]);
            // Draining again finds nothing: delivery is one-shot.
            assert!(take_all(wt.path()).is_empty());
        });
    }

    #[test]
    fn append_rejects_item_and_byte_limit_overflow() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            for i in 0..MAX_LIVE_QUEUE_ITEMS {
                append(wt.path(), &format!("p{i}")).unwrap();
            }
            let err = append(wt.path(), "one too many").unwrap_err().to_string();
            assert!(err.contains("live prompt queue is full"));
            assert_eq!(take_all(wt.path()).len(), MAX_LIVE_BATCH_ITEMS);

            let wt = tempfile::tempdir().unwrap();
            append(wt.path(), &"x".repeat(MAX_LIVE_QUEUE_BYTES)).unwrap();
            let err = append(wt.path(), "y").unwrap_err().to_string();
            assert!(err.contains("limit is"));
        });
    }

    #[test]
    fn take_all_drains_only_one_bounded_batch() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            for i in 0..(MAX_LIVE_BATCH_ITEMS + 2) {
                append(wt.path(), &format!("p{i}")).unwrap();
            }
            assert_eq!(take_all(wt.path()), vec!["p0", "p1", "p2", "p3"]);
            assert_eq!(take_all(wt.path()), vec!["p4", "p5"]);
            assert!(take_all(wt.path()).is_empty());
        });
    }

    #[test]
    fn take_all_splits_a_batch_at_the_byte_limit() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let large = "x".repeat(MAX_LIVE_BATCH_BYTES - 1);
            append(wt.path(), &large).unwrap();
            append(wt.path(), "yy").unwrap();
            assert_eq!(take_all(wt.path()), vec![large]);
            assert_eq!(take_all(wt.path()), vec!["yy"]);
        });
    }

    #[test]
    fn legacy_vec_file_is_migrated_to_item_files_on_drain() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let key = key(wt.path());
            let dir = dir(PROMPT_SUBDIR).unwrap();
            let legacy = dir.join(file_name(&key));
            json_file::write_atomic(
                &dir,
                &legacy,
                &LivePromptFile {
                    worktree: key.clone(),
                    prompts: vec!["old-1".to_string(), "old-2".to_string()],
                },
            )
            .unwrap();
            assert_eq!(take_all(wt.path()), vec!["old-1", "old-2"]);
            assert!(
                !legacy.exists(),
                "empty migrated queue is removed after drain"
            );
        });
    }

    #[test]
    fn legacy_vec_migration_truncates_at_the_documented_limits() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let key = key(wt.path());
            let dir = dir(PROMPT_SUBDIR).unwrap();
            let legacy = dir.join(file_name(&key));
            json_file::write_atomic(
                &dir,
                &legacy,
                &LivePromptFile {
                    worktree: key,
                    prompts: vec!["x".to_string(); MAX_LIVE_QUEUE_ITEMS + 1],
                },
            )
            .unwrap();
            let mut taken = Vec::new();
            loop {
                let batch = take_all(wt.path());
                if batch.is_empty() {
                    break;
                }
                taken.extend(batch);
            }
            assert_eq!(taken.len(), MAX_LIVE_QUEUE_ITEMS);
        });
    }

    #[test]
    fn concurrent_writers_preserve_every_prompt_once() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let path = wt.path().to_path_buf();
            let writers = 8;
            let barrier = Arc::new(Barrier::new(writers));
            let handles: Vec<_> = (0..writers)
                .map(|i| {
                    let barrier = Arc::clone(&barrier);
                    let path = path.clone();
                    thread::spawn(move || {
                        barrier.wait();
                        append(&path, &format!("p{i}")).unwrap();
                    })
                })
                .collect();
            for handle in handles {
                handle.join().unwrap();
            }
            let mut seen = Vec::new();
            loop {
                let batch = take_all(wt.path());
                if batch.is_empty() {
                    break;
                }
                seen.extend(batch);
            }
            seen.sort();
            assert_eq!(
                seen,
                (0..writers).map(|i| format!("p{i}")).collect::<Vec<_>>()
            );
        });
    }

    #[test]
    fn append_and_take_all_create_the_shared_store_lock() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            append(wt.path(), "queued").unwrap();
            // The per-store lock file lives alongside the prompt files, so a
            // concurrent append/drain in another process serialises behind it.
            let dir = dir(PROMPT_SUBDIR).unwrap();
            assert!(StoreLock::path(&dir).exists());
            assert_eq!(take_all(wt.path()), vec!["queued"]);
        });
    }

    #[test]
    fn clear_discards_queued_prompts_without_delivering_them() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            append(wt.path(), "for the old session").unwrap();
            clear(wt.path());
            // Gone: a session recreated at the same path does not inherit them.
            assert!(take_all(wt.path()).is_empty());
            // Clearing again (nothing queued) is a harmless no-op.
            clear(wt.path());
        });
    }

    #[test]
    fn distinct_worktrees_queue_independently() {
        with_data_dir(|_| {
            let a = tempfile::tempdir().unwrap();
            let b = tempfile::tempdir().unwrap();
            append(a.path(), "for a").unwrap();
            append(b.path(), "for b").unwrap();
            assert_eq!(take_all(a.path()), vec!["for a"]);
            assert_eq!(take_all(b.path()), vec!["for b"]);
        });
    }

    #[test]
    fn a_file_queued_for_another_worktree_reads_as_absent_and_is_preserved() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let other = tempfile::tempdir().unwrap();
            // Forge a file at wt's hashed name but stamped with a different
            // worktree, as a hash collision or a synced stale file would be.
            let dir = dir(PROMPT_SUBDIR).unwrap();
            let path = dir.join(file_name(&key(wt.path())));
            json_file::write_atomic(
                &dir,
                &path,
                &LivePromptFile {
                    worktree: key(other.path()),
                    prompts: vec!["not ours".to_string()],
                },
            )
            .unwrap();
            // Not returned for wt, and left intact for its rightful owner.
            assert!(take_all(wt.path()).is_empty());
            assert!(path.exists());
            let still: LivePromptFile = json_file::read(&path).unwrap().unwrap();
            assert_eq!(still.worktree, key(other.path()));
            assert_eq!(still.prompts, vec!["not ours".to_string()]);
        });
    }

    #[test]
    fn append_over_another_worktrees_file_starts_a_fresh_queue_stamped_as_ours() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let other = tempfile::tempdir().unwrap();
            let dir = dir(PROMPT_SUBDIR).unwrap();
            let path = dir.join(file_name(&key(wt.path())));
            // A colliding file stamped for another worktree sits at wt's name.
            json_file::write_atomic(
                &dir,
                &path,
                &LivePromptFile {
                    worktree: key(other.path()),
                    prompts: vec!["theirs".to_string()],
                },
            )
            .unwrap();
            // Appending as wt does not adopt their prompts; it restamps the file
            // as ours with only our prompt, so we can never read theirs back.
            append(wt.path(), "ours").unwrap();
            assert_eq!(take_all(wt.path()), vec!["ours"]);
        });
    }

    #[test]
    fn a_corrupt_file_reads_as_absent_and_is_cleared() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let dir = dir(PROMPT_SUBDIR).unwrap();
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join(file_name(&key(wt.path())));
            fs::write(&path, "not json at all").unwrap();
            assert!(take_all(wt.path()).is_empty());
            assert!(!path.exists());
        });
    }

    #[test]
    fn append_after_a_corrupt_file_starts_a_fresh_queue() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let dir = dir(PROMPT_SUBDIR).unwrap();
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join(file_name(&key(wt.path())));
            fs::write(&path, "}{ not json").unwrap();
            // Append treats the unreadable file as an empty queue and overwrites it.
            append(wt.path(), "recovered").unwrap();
            assert_eq!(take_all(wt.path()), vec!["recovered"]);
        });
    }

    #[test]
    fn requeue_restores_undelivered_prompts_for_a_later_take() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            // A batch is drained, but delivery fails after the first prompt, so the
            // undelivered tail is put back — and a later drain hands it back in order.
            append(wt.path(), "first").unwrap();
            append(wt.path(), "second").unwrap();
            append(wt.path(), "third").unwrap();
            let taken = take_all(wt.path());
            assert_eq!(taken, vec!["first", "second", "third"]);
            // "first" delivered; "second"/"third" undelivered and returned.
            requeue(wt.path(), &taken[1..]).unwrap();
            assert_eq!(take_all(wt.path()), vec!["second", "third"]);
        });
    }

    #[test]
    fn requeue_places_undelivered_prompts_before_ones_appended_since() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            // A prompt is taken then fails to deliver; meanwhile a new send arrives.
            append(wt.path(), "taken").unwrap();
            let taken = take_all(wt.path());
            assert_eq!(taken, vec!["taken"]);
            append(wt.path(), "arrived-since").unwrap();
            // Requeuing puts the retried prompt ahead of the newer one, so overall
            // send order (taken before arrived-since) is preserved.
            requeue(wt.path(), &taken).unwrap();
            assert_eq!(take_all(wt.path()), vec!["taken", "arrived-since"]);
        });
    }

    #[test]
    fn requeue_of_nothing_is_a_no_op() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            append(wt.path(), "kept").unwrap();
            // An empty requeue leaves the existing queue untouched and writes nothing.
            requeue(wt.path(), &[]).unwrap();
            assert_eq!(take_all(wt.path()), vec!["kept"]);
        });
    }

    #[test]
    fn requeue_preserves_already_accepted_work_across_temporary_limit_overflow() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let too_many = vec!["x".to_string(); MAX_LIVE_QUEUE_ITEMS + 1];
            requeue(wt.path(), &too_many).unwrap();
            let err = append(wt.path(), "blocked until drain")
                .unwrap_err()
                .to_string();
            assert!(err.contains("items"));
            let mut restored = Vec::new();
            while has_queued(wt.path()).unwrap() {
                restored.extend(take_all(wt.path()));
            }
            assert_eq!(restored, too_many);

            let too_large = vec!["x".repeat(MAX_LIVE_QUEUE_BYTES + 1)];
            requeue(wt.path(), &too_large).unwrap();
            let err = append(wt.path(), "blocked until drain")
                .unwrap_err()
                .to_string();
            assert!(err.contains("bytes"));
            assert_eq!(take_all(wt.path()), too_large);
        });
    }

    #[test]
    fn any_queued_reports_whether_a_live_prompt_is_waiting() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            // Nothing queued (the directory may not even exist yet).
            assert!(!any_queued());
            // A queued prompt makes it report true.
            append(wt.path(), "queued").unwrap();
            assert!(any_queued());
            // Draining it (the file removed) makes it report empty again, even
            // though the shared store-lock file remains in the directory.
            let _ = take_all(wt.path());
            assert!(StoreLock::path(&dir(PROMPT_SUBDIR).unwrap()).exists());
            assert!(!any_queued());
        });
    }

    #[test]
    fn requeue_does_not_adopt_another_worktrees_prompts() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let other = tempfile::tempdir().unwrap();
            // A colliding file stamped for another worktree sits at wt's name.
            let dir = dir(PROMPT_SUBDIR).unwrap();
            let path = dir.join(file_name(&key(wt.path())));
            json_file::write_atomic(
                &dir,
                &path,
                &LivePromptFile {
                    worktree: key(other.path()),
                    prompts: vec!["theirs".to_string()],
                },
            )
            .unwrap();
            // Requeuing as wt restamps the file as ours with only our prompt; the
            // other worktree's prompt is never adopted.
            requeue(wt.path(), &["ours".to_string()]).unwrap();
            assert_eq!(take_all(wt.path()), vec!["ours"]);
        });
    }

    #[test]
    fn spawn_take_is_strict_empty_and_claims_only_a_bounded_ready_batch() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let now = UNIX_EPOCH + Duration::from_secs(10_000);
            assert_eq!(
                take_ready_for_spawn(wt.path(), now).unwrap(),
                TakeReadyForSpawn::Empty
            );
            for i in 0..(MAX_LIVE_BATCH_ITEMS + 2) {
                append(wt.path(), &format!("p{i}")).unwrap();
            }

            let first = ready_batch(take_ready_for_spawn(wt.path(), now).unwrap());
            assert_eq!(first.prompts, vec!["p0", "p1", "p2", "p3"]);
            assert_eq!(first.retry, None);
            let second = ready_batch(take_ready_for_spawn(wt.path(), now).unwrap());
            assert_eq!(second.prompts, vec!["p4", "p5"]);
            assert_eq!(second.retry, None);
            assert_eq!(
                take_ready_for_spawn(wt.path(), now).unwrap(),
                TakeReadyForSpawn::Empty
            );
        });
    }

    #[test]
    fn spawn_failure_waits_then_returns_the_retry_state_with_the_ready_batch() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let now = UNIX_EPOCH + Duration::from_secs(20_000);
            append(wt.path(), "retry me").unwrap();
            let taken = ready_batch(take_ready_for_spawn(wt.path(), now).unwrap());
            let retry =
                requeue_after_spawn_failure(wt.path(), &taken, "spawn failed", now).unwrap();
            assert_eq!(retry.attempts, 1);
            assert!(!retry.dead);
            assert_eq!(
                take_ready_for_spawn(wt.path(), now).unwrap(),
                TakeReadyForSpawn::Waiting(retry.clone())
            );
            assert!(has_queued(wt.path()).unwrap());

            let retry_at = UNIX_EPOCH + Duration::from_secs(retry.next_retry_unix_secs);
            let retried = ready_batch(take_ready_for_spawn(wt.path(), retry_at).unwrap());
            assert_eq!(retried.prompts, vec!["retry me"]);
            assert_eq!(retried.retry, Some(retry));
        });
    }

    #[test]
    fn repeated_spawn_failures_dead_letter_and_leave_the_queue_durable() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let mut now = UNIX_EPOCH + Duration::from_secs(30_000);
            append(wt.path(), "eventually dead").unwrap();
            let mut taken = ready_batch(take_ready_for_spawn(wt.path(), now).unwrap());
            let mut final_retry = None;
            for attempt in 1..=MAX_PROMPT_RETRY_ATTEMPTS {
                let retry = requeue_after_spawn_failure(
                    wt.path(),
                    &taken,
                    &format!("failure {attempt}"),
                    now,
                )
                .unwrap();
                assert_eq!(retry.attempts, attempt);
                if retry.dead {
                    final_retry = Some(retry);
                    break;
                }
                assert_eq!(
                    take_ready_for_spawn(wt.path(), now).unwrap(),
                    TakeReadyForSpawn::Waiting(retry.clone())
                );
                now = UNIX_EPOCH + Duration::from_secs(retry.next_retry_unix_secs);
                taken = ready_batch(take_ready_for_spawn(wt.path(), now).unwrap());
            }
            let final_retry = final_retry.expect("retry limit should dead-letter");
            assert_eq!(
                take_ready_for_spawn(wt.path(), now).unwrap(),
                TakeReadyForSpawn::Dead(final_retry)
            );
            assert!(has_queued(wt.path()).unwrap());
        });
    }

    #[test]
    fn cancel_requeue_preserves_retry_and_precedes_concurrent_append() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let now = UNIX_EPOCH + Duration::from_secs(40_000);
            append(wt.path(), "oldest").unwrap();
            let initial = ready_batch(take_ready_for_spawn(wt.path(), now).unwrap());
            let retry =
                requeue_after_spawn_failure(wt.path(), &initial, "first failure", now).unwrap();
            let retry_at = UNIX_EPOCH + Duration::from_secs(retry.next_retry_unix_secs);
            let claimed = ready_batch(take_ready_for_spawn(wt.path(), retry_at).unwrap());
            append(wt.path(), "newer").unwrap();

            requeue_taken(wt.path(), &claimed).unwrap();
            let restored = ready_batch(take_ready_for_spawn(wt.path(), retry_at).unwrap());
            assert_eq!(restored.prompts, vec!["oldest"]);
            assert_eq!(restored.retry, Some(retry));
            let newer = ready_batch(take_ready_for_spawn(wt.path(), retry_at).unwrap());
            assert_eq!(newer.prompts, vec!["newer"]);
            assert_eq!(newer.retry, None);
        });
    }

    #[test]
    fn retry_boundary_excludes_new_append_and_only_old_batch_dead_letters() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let mut now = UNIX_EPOCH + Duration::from_secs(45_000);
            append(wt.path(), "old-retry").unwrap();
            let mut taken = ready_batch(take_ready_for_spawn(wt.path(), now).unwrap());
            let mut retry = None;
            for attempt in 1..=4 {
                let state = requeue_after_spawn_failure(
                    wt.path(),
                    &taken,
                    &format!("failure {attempt}"),
                    now,
                )
                .unwrap();
                assert_eq!(state.attempts, attempt);
                retry = Some(state.clone());
                if attempt < 4 {
                    now = UNIX_EPOCH + Duration::from_secs(state.next_retry_unix_secs);
                    taken = ready_batch(take_ready_for_spawn(wt.path(), now).unwrap());
                }
            }
            let retry = retry.unwrap();
            append(wt.path(), "new-during-backoff").unwrap();
            assert_eq!(
                take_ready_for_spawn(wt.path(), now).unwrap(),
                TakeReadyForSpawn::Waiting(retry.clone())
            );

            let queue = queue_dir(&dir(PROMPT_SUBDIR).unwrap(), &key(wt.path()));
            let value: serde_json::Value = json_file::read(&meta_path(&queue)).unwrap().unwrap();
            assert_eq!(value["retry_count"], 1);
            assert_eq!(value["count"], 2);

            now = UNIX_EPOCH + Duration::from_secs(retry.next_retry_unix_secs);
            let old = ready_batch(take_ready_for_spawn(wt.path(), now).unwrap());
            assert_eq!(old.prompts, vec!["old-retry"]);
            assert_eq!(old.retry, Some(retry));
            let value: serde_json::Value = json_file::read(&meta_path(&queue)).unwrap().unwrap();
            assert_eq!(value["retry"], serde_json::Value::Null);
            assert_eq!(value["retry_count"], 0);

            let dead = requeue_after_spawn_failure(wt.path(), &old, "fifth failure", now).unwrap();
            assert!(dead.dead);
            let new = ready_batch(take_ready_for_spawn(wt.path(), now).unwrap());
            assert_eq!(new.prompts, vec!["new-during-backoff"]);
            assert_eq!(new.retry, None);
            assert_eq!(
                take_ready_for_spawn(wt.path(), now).unwrap(),
                TakeReadyForSpawn::Dead(dead)
            );
        });
    }

    #[test]
    fn dead_batch_does_not_block_new_active_spawn_and_has_stable_serde_shape() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let mut now = UNIX_EPOCH + Duration::from_secs(50_000);
            append(wt.path(), "dead-old").unwrap();
            let mut taken = ready_batch(take_ready_for_spawn(wt.path(), now).unwrap());
            for attempt in 1..=MAX_PROMPT_RETRY_ATTEMPTS {
                let retry = requeue_after_spawn_failure(
                    wt.path(),
                    &taken,
                    &format!("failure {attempt}"),
                    now,
                )
                .unwrap();
                if retry.dead {
                    break;
                }
                now = UNIX_EPOCH + Duration::from_secs(retry.next_retry_unix_secs);
                taken = ready_batch(take_ready_for_spawn(wt.path(), now).unwrap());
            }
            assert!(matches!(
                take_ready_for_spawn(wt.path(), now).unwrap(),
                TakeReadyForSpawn::Dead(_)
            ));
            assert!(has_queued(wt.path()).unwrap());

            let queue = queue_dir(&dir(PROMPT_SUBDIR).unwrap(), &key(wt.path()));
            let value: serde_json::Value = json_file::read(&meta_path(&queue)).unwrap().unwrap();
            let dead = value["dead_batches"].as_array().unwrap();
            assert_eq!(dead.len(), 1);
            assert_eq!(dead[0]["prompts"], serde_json::json!(["dead-old"]));
            assert_eq!(dead[0]["retry"]["dead"], true);

            append(wt.path(), "new-active").unwrap();
            let active = ready_batch(take_ready_for_spawn(wt.path(), now).unwrap());
            assert_eq!(active.prompts, vec!["new-active"]);
            assert_eq!(active.retry, None);
            assert!(matches!(
                take_ready_for_spawn(wt.path(), now).unwrap(),
                TakeReadyForSpawn::Dead(_)
            ));
        });
    }

    #[test]
    fn live_drain_promotes_dead_work_and_preserves_order_on_delivery_requeue() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let mut now = UNIX_EPOCH + Duration::from_secs(55_000);
            for i in 0..MAX_LIVE_BATCH_ITEMS {
                append(wt.path(), &format!("dead-{i}")).unwrap();
            }
            let mut taken = ready_batch(take_ready_for_spawn(wt.path(), now).unwrap());
            for attempt in 1..=MAX_PROMPT_RETRY_ATTEMPTS {
                let retry = requeue_after_spawn_failure(
                    wt.path(),
                    &taken,
                    &format!("failure {attempt}"),
                    now,
                )
                .unwrap();
                if retry.dead {
                    break;
                }
                now = UNIX_EPOCH + Duration::from_secs(retry.next_retry_unix_secs);
                taken = ready_batch(take_ready_for_spawn(wt.path(), now).unwrap());
            }
            append(wt.path(), "new-0").unwrap();
            append(wt.path(), "new-1").unwrap();

            let live = take_batch(wt.path());
            assert_eq!(live, vec!["dead-0", "dead-1", "dead-2", "dead-3"]);
            // Simulate only the first PTY write succeeding. The undelivered dead
            // tail must stay ahead of active work promoted during the take.
            requeue(wt.path(), &live[1..]).unwrap();
            assert_eq!(
                take_batch(wt.path()),
                vec!["dead-1", "dead-2", "dead-3", "new-0"]
            );
            assert_eq!(take_batch(wt.path()), vec!["new-1"]);
            assert!(!has_queued(wt.path()).unwrap());
        });
    }

    #[test]
    fn old_metadata_without_retry_deserializes_as_ready() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            append(wt.path(), "legacy metadata").unwrap();
            let queue = queue_dir(&dir(PROMPT_SUBDIR).unwrap(), &key(wt.path()));
            let path = meta_path(&queue);
            let mut value: serde_json::Value = json_file::read(&path).unwrap().unwrap();
            value.as_object_mut().unwrap().remove("retry");
            value.as_object_mut().unwrap().remove("retry_count");
            value.as_object_mut().unwrap().remove("dead_batches");
            json_file::write_atomic(&queue, &path, &value).unwrap();

            let taken = ready_batch(
                take_ready_for_spawn(wt.path(), UNIX_EPOCH + Duration::from_secs(60_000)).unwrap(),
            );
            assert_eq!(taken.prompts, vec!["legacy metadata"]);
            assert_eq!(taken.retry, None);
        });
    }

    #[test]
    fn append_migrates_legacy_dead_retry_before_adding_independent_active_work() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            append(wt.path(), "legacy-dead").unwrap();
            let queue = queue_dir(&dir(PROMPT_SUBDIR).unwrap(), &key(wt.path()));
            let path = meta_path(&queue);
            let mut value: serde_json::Value = json_file::read(&path).unwrap().unwrap();
            value.as_object_mut().unwrap().remove("dead_batches");
            value["retry"] = serde_json::to_value(RetryState {
                attempts: MAX_PROMPT_RETRY_ATTEMPTS,
                next_retry_unix_secs: 90_000,
                last_error: "legacy dead".to_string(),
                dead: true,
            })
            .unwrap();
            json_file::write_atomic(&queue, &path, &value).unwrap();

            append(wt.path(), "new-active").unwrap();
            let now = UNIX_EPOCH + Duration::from_secs(90_000);
            let active = ready_batch(take_ready_for_spawn(wt.path(), now).unwrap());
            assert_eq!(active.prompts, vec!["new-active"]);
            assert!(matches!(
                take_ready_for_spawn(wt.path(), now).unwrap(),
                TakeReadyForSpawn::Dead(retry) if retry.last_error == "legacy dead"
            ));
            assert_eq!(take_batch(wt.path()), vec!["legacy-dead"]);
        });
    }

    #[test]
    fn legacy_missing_retry_boundary_applies_only_to_one_bounded_prefix() {
        with_data_dir(|_| {
            let now = UNIX_EPOCH + Duration::from_secs(95_000);

            let dead_wt = tempfile::tempdir().unwrap();
            for i in 0..(MAX_LIVE_BATCH_ITEMS + 2) {
                append(dead_wt.path(), &format!("dead-old-{i}")).unwrap();
            }
            let queue = queue_dir(&dir(PROMPT_SUBDIR).unwrap(), &key(dead_wt.path()));
            let path = meta_path(&queue);
            let mut value: serde_json::Value = json_file::read(&path).unwrap().unwrap();
            value.as_object_mut().unwrap().remove("retry_count");
            value.as_object_mut().unwrap().remove("dead_batches");
            value["retry"] = serde_json::to_value(RetryState {
                attempts: MAX_PROMPT_RETRY_ATTEMPTS,
                next_retry_unix_secs: 95_000,
                last_error: "legacy dead".to_string(),
                dead: true,
            })
            .unwrap();
            json_file::write_atomic(&queue, &path, &value).unwrap();

            let active = ready_batch(take_ready_for_spawn(dead_wt.path(), now).unwrap());
            assert_eq!(active.prompts, vec!["dead-old-4", "dead-old-5"]);
            assert_eq!(active.retry, None);
            assert!(matches!(
                take_ready_for_spawn(dead_wt.path(), now).unwrap(),
                TakeReadyForSpawn::Dead(retry) if retry.last_error == "legacy dead"
            ));
            assert_eq!(
                take_batch(dead_wt.path()),
                vec!["dead-old-0", "dead-old-1", "dead-old-2", "dead-old-3"]
            );

            let retry_wt = tempfile::tempdir().unwrap();
            for i in 0..(MAX_LIVE_BATCH_ITEMS + 2) {
                append(retry_wt.path(), &format!("retry-old-{i}")).unwrap();
            }
            let queue = queue_dir(&dir(PROMPT_SUBDIR).unwrap(), &key(retry_wt.path()));
            let path = meta_path(&queue);
            let mut value: serde_json::Value = json_file::read(&path).unwrap().unwrap();
            value.as_object_mut().unwrap().remove("retry_count");
            value["retry"] = serde_json::to_value(RetryState {
                attempts: 2,
                next_retry_unix_secs: 95_000,
                last_error: "legacy retry".to_string(),
                dead: false,
            })
            .unwrap();
            json_file::write_atomic(&queue, &path, &value).unwrap();

            let retried = ready_batch(take_ready_for_spawn(retry_wt.path(), now).unwrap());
            assert_eq!(
                retried.prompts,
                vec!["retry-old-0", "retry-old-1", "retry-old-2", "retry-old-3"]
            );
            assert_eq!(retried.retry.unwrap().attempts, 2);
            let remaining = ready_batch(take_ready_for_spawn(retry_wt.path(), now).unwrap());
            assert_eq!(remaining.prompts, vec!["retry-old-4", "retry-old-5"]);
            assert_eq!(remaining.retry, None);
        });
    }

    #[test]
    fn strict_spawn_take_reports_corrupt_items_without_removing_the_queue() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            append(wt.path(), "keep me").unwrap();
            let queue = queue_dir(&dir(PROMPT_SUBDIR).unwrap(), &key(wt.path()));
            let item = item_paths(&queue).into_iter().next().unwrap().1;
            fs::write(&item, "not json").unwrap();

            let error = take_ready_for_spawn(wt.path(), UNIX_EPOCH + Duration::from_secs(70_000))
                .unwrap_err()
                .to_string();
            assert!(error.contains("expected") || error.contains("json"));
            assert!(queue.exists());
            assert!(item.exists());
        });
    }

    #[test]
    fn strict_spawn_take_validates_dead_and_total_metadata() {
        with_data_dir(|_| {
            let now = UNIX_EPOCH + Duration::from_secs(75_000);

            let invalid_dead = tempfile::tempdir().unwrap();
            append(invalid_dead.path(), "active").unwrap();
            let queue = queue_dir(&dir(PROMPT_SUBDIR).unwrap(), &key(invalid_dead.path()));
            let mut meta = read_meta(&queue, &key(invalid_dead.path())).unwrap();
            meta.dead_batches.push(DeadLivePromptBatch {
                prompts: vec!["dead".to_string()],
                retry: RetryState {
                    attempts: 1,
                    next_retry_unix_secs: 0,
                    last_error: "not actually dead".to_string(),
                    dead: false,
                },
            });
            write_meta(&queue, &meta).unwrap();
            assert!(has_queued(invalid_dead.path())
                .unwrap_err()
                .to_string()
                .contains("inconsistent"));
            assert!(take_ready_for_spawn(invalid_dead.path(), now)
                .unwrap_err()
                .to_string()
                .contains("invalid dead batch"));

            let count_underflow = tempfile::tempdir().unwrap();
            append(count_underflow.path(), "active").unwrap();
            let queue = queue_dir(&dir(PROMPT_SUBDIR).unwrap(), &key(count_underflow.path()));
            let mut meta = read_meta(&queue, &key(count_underflow.path())).unwrap();
            meta.dead_batches.push(DeadLivePromptBatch {
                prompts: vec!["one".to_string(), "two".to_string()],
                retry: RetryState {
                    attempts: MAX_PROMPT_RETRY_ATTEMPTS,
                    next_retry_unix_secs: 0,
                    last_error: "dead".to_string(),
                    dead: true,
                },
            });
            write_meta(&queue, &meta).unwrap();
            assert!(take_ready_for_spawn(count_underflow.path(), now)
                .unwrap_err()
                .to_string()
                .contains("fewer total items"));

            let byte_underflow = tempfile::tempdir().unwrap();
            append(byte_underflow.path(), "a").unwrap();
            let queue = queue_dir(&dir(PROMPT_SUBDIR).unwrap(), &key(byte_underflow.path()));
            let mut meta = read_meta(&queue, &key(byte_underflow.path())).unwrap();
            meta.dead_batches.push(DeadLivePromptBatch {
                prompts: vec!["long-dead".to_string()],
                retry: RetryState {
                    attempts: MAX_PROMPT_RETRY_ATTEMPTS,
                    next_retry_unix_secs: 0,
                    last_error: "dead".to_string(),
                    dead: true,
                },
            });
            write_meta(&queue, &meta).unwrap();
            assert!(take_ready_for_spawn(byte_underflow.path(), now)
                .unwrap_err()
                .to_string()
                .contains("fewer total bytes"));

            let active_mismatch = tempfile::tempdir().unwrap();
            append(active_mismatch.path(), "a").unwrap();
            let queue = queue_dir(&dir(PROMPT_SUBDIR).unwrap(), &key(active_mismatch.path()));
            let mut meta = read_meta(&queue, &key(active_mismatch.path())).unwrap();
            meta.count += 1;
            write_meta(&queue, &meta).unwrap();
            assert!(take_ready_for_spawn(active_mismatch.path(), now)
                .unwrap_err()
                .to_string()
                .contains("active item files"));

            let byte_mismatch = tempfile::tempdir().unwrap();
            append(byte_mismatch.path(), "a").unwrap();
            let queue = queue_dir(&dir(PROMPT_SUBDIR).unwrap(), &key(byte_mismatch.path()));
            let mut meta = read_meta(&queue, &key(byte_mismatch.path())).unwrap();
            meta.bytes += 1;
            write_meta(&queue, &meta).unwrap();
            assert!(take_ready_for_spawn(byte_mismatch.path(), now)
                .unwrap_err()
                .to_string()
                .contains("active payload bytes"));
        });
    }

    #[test]
    fn strict_spawn_take_validates_retry_metadata_boundaries() {
        with_data_dir(|_| {
            let now = UNIX_EPOCH + Duration::from_secs(76_000);
            let retry = RetryState {
                attempts: 1,
                next_retry_unix_secs: 0,
                last_error: "retry".to_string(),
                dead: false,
            };

            let count_without_state = tempfile::tempdir().unwrap();
            append(count_without_state.path(), "active").unwrap();
            let queue = queue_dir(
                &dir(PROMPT_SUBDIR).unwrap(),
                &key(count_without_state.path()),
            );
            let mut meta = read_meta(&queue, &key(count_without_state.path())).unwrap();
            meta.retry_count = 1;
            write_meta(&queue, &meta).unwrap();
            assert!(take_ready_for_spawn(count_without_state.path(), now)
                .unwrap_err()
                .to_string()
                .contains("without retry state"));

            let state_without_items = tempfile::tempdir().unwrap();
            append(state_without_items.path(), "remove me").unwrap();
            let queue = queue_dir(
                &dir(PROMPT_SUBDIR).unwrap(),
                &key(state_without_items.path()),
            );
            for (_, path) in item_paths(&queue) {
                fs::remove_file(path).unwrap();
            }
            let mut meta = read_meta(&queue, &key(state_without_items.path())).unwrap();
            meta.count = 0;
            meta.bytes = 0;
            meta.retry = Some(retry.clone());
            write_meta(&queue, &meta).unwrap();
            assert!(take_ready_for_spawn(state_without_items.path(), now)
                .unwrap_err()
                .to_string()
                .contains("retry state without active items"));

            let boundary_overflow = tempfile::tempdir().unwrap();
            append(boundary_overflow.path(), "only one").unwrap();
            let queue = queue_dir(&dir(PROMPT_SUBDIR).unwrap(), &key(boundary_overflow.path()));
            let mut meta = read_meta(&queue, &key(boundary_overflow.path())).unwrap();
            meta.retry = Some(retry.clone());
            meta.retry_count = 2;
            write_meta(&queue, &meta).unwrap();
            assert!(take_ready_for_spawn(boundary_overflow.path(), now)
                .unwrap_err()
                .to_string()
                .contains("but only 1 active items"));

            let root = dir(PROMPT_SUBDIR).unwrap();
            let rewrite = tempfile::tempdir().unwrap();
            let rewrite_key = key(rewrite.path());
            assert!(rewrite_queue_locked(
                &root,
                &rewrite_key,
                vec!["active".to_string()],
                None,
                1,
                Vec::new(),
            )
            .err()
            .unwrap()
            .to_string()
            .contains("without retry state"));
            assert!(rewrite_queue_locked(
                &root,
                &rewrite_key,
                vec!["active".to_string()],
                Some(retry),
                2,
                Vec::new(),
            )
            .err()
            .unwrap()
            .to_string()
            .contains("for 1 active prompts"));
        });
    }

    #[test]
    fn bounded_batch_stops_before_crossing_the_byte_limit() {
        let first = "x".repeat(MAX_LIVE_BATCH_BYTES - 1);
        let items = vec![
            (PathBuf::from("first"), first.clone()),
            (PathBuf::from("second"), "yy".to_string()),
        ];
        let (prompts, paths, bytes) = bounded_batch(&items);
        assert_eq!(prompts, vec![first]);
        assert_eq!(paths, vec![PathBuf::from("first")]);
        assert_eq!(bytes, MAX_LIVE_BATCH_BYTES - 1);
    }

    #[test]
    fn strict_queue_operations_reject_corrupt_metadata_without_claiming_items() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            append(wt.path(), "durable").unwrap();
            let queue = queue_dir(&dir(PROMPT_SUBDIR).unwrap(), &key(wt.path()));
            let metadata = meta_path(&queue);
            fs::write(&metadata, "not json").unwrap();

            assert!(has_queued(wt.path())
                .unwrap_err()
                .to_string()
                .contains("unreadable"));
            assert!(take_ready_for_spawn(wt.path(), UNIX_EPOCH)
                .unwrap_err()
                .to_string()
                .contains("unreadable"));
            assert!(requeue(wt.path(), &["restore".to_string()])
                .unwrap_err()
                .to_string()
                .contains("unreadable"));

            let taken = TakenLivePromptBatch {
                prompts: vec!["claimed".to_string()],
                retry: None,
            };
            assert!(requeue_taken(wt.path(), &taken)
                .unwrap_err()
                .to_string()
                .contains("unreadable"));

            let almost_dead = TakenLivePromptBatch {
                prompts: vec!["failed".to_string()],
                retry: Some(RetryState {
                    attempts: MAX_PROMPT_RETRY_ATTEMPTS - 1,
                    next_retry_unix_secs: 0,
                    last_error: "previous".to_string(),
                    dead: false,
                }),
            };
            assert!(requeue_after_spawn_failure(
                wt.path(),
                &almost_dead,
                "terminal failure",
                UNIX_EPOCH,
            )
            .unwrap_err()
            .to_string()
            .contains("unreadable"));

            assert!(queue.exists());
            assert_eq!(item_paths(&queue).len(), 1);
        });
    }

    #[test]
    fn requeueing_an_empty_spawn_claim_is_a_noop() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            requeue_taken(
                wt.path(),
                &TakenLivePromptBatch {
                    prompts: Vec::new(),
                    retry: None,
                },
            )
            .unwrap();
            assert!(!has_queued(wt.path()).unwrap());
        });
    }

    #[test]
    #[should_panic(expected = "expected ready live-prompt batch")]
    fn ready_batch_helper_rejects_non_ready_results() {
        let _ = ready_batch(TakeReadyForSpawn::Empty);
    }

    #[test]
    fn stateful_spawn_requeues_are_lossless_when_concurrent_append_refills_capacity() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            append(wt.path(), "claimed").unwrap();
            let claimed = ready_batch(
                take_ready_for_spawn(wt.path(), UNIX_EPOCH + Duration::from_secs(80_000)).unwrap(),
            );
            for i in 0..MAX_LIVE_QUEUE_ITEMS {
                append(wt.path(), &format!("new-{i}")).unwrap();
            }
            requeue_taken(wt.path(), &claimed).unwrap();
            let error = append(wt.path(), "must wait for capacity")
                .unwrap_err()
                .to_string();
            assert!(error.contains("items"));
            let mut restored = Vec::new();
            while has_queued(wt.path()).unwrap() {
                restored.extend(take_all(wt.path()));
            }
            assert_eq!(restored.first().unwrap(), "claimed");
            assert_eq!(restored.len(), MAX_LIVE_QUEUE_ITEMS + 1);

            append(wt.path(), "failed-claim").unwrap();
            let now = UNIX_EPOCH + Duration::from_secs(81_000);
            let failed = ready_batch(take_ready_for_spawn(wt.path(), now).unwrap());
            for i in 0..MAX_LIVE_QUEUE_ITEMS {
                append(wt.path(), &format!("after-failure-{i}")).unwrap();
            }
            let retry = requeue_after_spawn_failure(
                wt.path(),
                &failed,
                "spawn failed after capacity refill",
                now,
            )
            .unwrap();
            assert_eq!(retry.attempts, 1);
            let error = append(wt.path(), "still blocked").unwrap_err().to_string();
            assert!(error.contains("items"));
            let mut restored = Vec::new();
            while has_queued(wt.path()).unwrap() {
                restored.extend(take_all(wt.path()));
            }
            assert_eq!(restored.first().unwrap(), "failed-claim");
            assert_eq!(restored.len(), MAX_LIVE_QUEUE_ITEMS + 1);
        });
    }
}

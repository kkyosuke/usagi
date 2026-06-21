//! Business logic for agent memories: save (upsert), read, update, delete, list
//! and search the memories stored under `<repo>/.usagi/memory/`.
//!
//! A memory is addressed by its `name`, a filename-safe slug. [`save`] is an
//! upsert: saving under a name that already exists updates that memory in place
//! (preserving its `created_at`) rather than creating a duplicate, which keeps a
//! single fact in a single place.

use std::path::Path;

use anyhow::Result;
use chrono::Utc;

use crate::domain::memory::{slugify, Memory, MemorySummary, MemoryType};
use crate::infrastructure::memory_store::MemoryStore;

mod view;

pub use view::{MemorySummaryView, MemoryView};

/// Fields needed to save a memory. The name is normalised to a slug and the
/// timestamps are assigned by [`save`].
pub struct NewMemory {
    pub name: String,
    pub title: String,
    pub kind: MemoryType,
    pub related: Vec<String>,
    pub body: String,
}

/// A partial update to an existing memory: every `Some` field is applied, every
/// `None` field is left unchanged.
#[derive(Default)]
pub struct MemoryChanges {
    pub title: Option<String>,
    pub kind: Option<MemoryType>,
    pub related: Option<Vec<String>>,
    pub body: Option<String>,
}

impl MemoryChanges {
    /// Whether this update would change anything.
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.kind.is_none() && self.related.is_none() && self.body.is_none()
    }
}

/// Filters applied to listings and searches. An unset field matches everything.
#[derive(Default)]
pub struct MemoryFilter {
    pub kind: Option<MemoryType>,
}

impl MemoryFilter {
    fn matches(&self, summary: &MemorySummary) -> bool {
        self.kind.is_none_or(|kind| summary.kind == kind)
    }
}

/// Save a memory under its (slugified) name. Updates the existing memory in place
/// when one already exists, otherwise creates a new one. Returns the stored
/// memory.
pub fn save(repo_root: &Path, new: NewMemory) -> Result<Memory> {
    let store = MemoryStore::new(repo_root);
    let name = slugify(&new.name);
    // Hold the lock across the read (to learn whether the memory exists and
    // preserve its `created_at`) and the write, so a concurrent `save` of the
    // same name cannot interleave between the two and clobber the result.
    let lock = store.lock()?;
    let now = Utc::now();
    let memory = match store.read(&name)? {
        Some(existing) => Memory {
            name,
            title: new.title,
            kind: new.kind,
            related: new.related,
            created_at: existing.created_at,
            updated_at: now,
            body: new.body,
        },
        None => Memory {
            name,
            title: new.title,
            kind: new.kind,
            related: new.related,
            created_at: now,
            updated_at: now,
            body: new.body,
        },
    };
    store.write_locked(&lock, &memory)?;
    Ok(memory)
}

/// Fetch a single memory by name.
pub fn get(repo_root: &Path, name: &str) -> Result<Option<Memory>> {
    MemoryStore::new(repo_root).read(&slugify(name))
}

/// List memories matching `filter`, newest first.
pub fn list(repo_root: &Path, filter: &MemoryFilter) -> Result<Vec<MemorySummary>> {
    let mut summaries = MemoryStore::new(repo_root).summaries()?;
    summaries.retain(|s| filter.matches(s));
    sort_newest_first(&mut summaries);
    Ok(summaries)
}

/// Full-text search memory names, titles and bodies (case-insensitive), then
/// apply `filter`. Results are newest first.
pub fn search(repo_root: &Path, query: &str, filter: &MemoryFilter) -> Result<Vec<MemorySummary>> {
    let memories = MemoryStore::new(repo_root).scan()?;
    // Case-fold with Unicode-aware `to_lowercase` so the fold works for the
    // Japanese text the UI carries.
    let needle = query.to_lowercase();
    let mut summaries: Vec<MemorySummary> = memories
        .into_iter()
        .filter(|m| {
            needle.is_empty()
                || m.name.to_lowercase().contains(&needle)
                || m.title.to_lowercase().contains(&needle)
                || m.body.to_lowercase().contains(&needle)
        })
        .map(|m| m.summary())
        .filter(|s| filter.matches(s))
        .collect();
    sort_newest_first(&mut summaries);
    Ok(summaries)
}

/// Apply `changes` to the memory with `name`. Returns the updated memory, or
/// `None` if no such memory exists.
pub fn update(repo_root: &Path, name: &str, changes: MemoryChanges) -> Result<Option<Memory>> {
    let store = MemoryStore::new(repo_root);
    // Hold the lock across the read and the write so a concurrent `update` or
    // `save` of the same memory cannot interleave between the two and clobber
    // this change (a lost update). Mirrors `save` above.
    let lock = store.lock()?;
    let Some(mut memory) = store.read(&slugify(name))? else {
        return Ok(None);
    };
    if let Some(title) = changes.title {
        memory.title = title;
    }
    if let Some(kind) = changes.kind {
        memory.kind = kind;
    }
    if let Some(related) = changes.related {
        memory.related = related;
    }
    if let Some(body) = changes.body {
        memory.body = body;
    }
    memory.updated_at = Utc::now();
    store.write_locked(&lock, &memory)?;
    Ok(Some(memory))
}

/// Delete the memory with `name`, returning whether it existed.
pub fn delete(repo_root: &Path, name: &str) -> Result<bool> {
    MemoryStore::new(repo_root).remove(&slugify(name))
}

/// Order summaries by recency (newest `updated_at` first), breaking ties by name
/// so the order is stable.
fn sort_newest_first(summaries: &mut [MemorySummary]) {
    summaries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.name.cmp(&b.name)));
}

#[cfg(test)]
mod tests;

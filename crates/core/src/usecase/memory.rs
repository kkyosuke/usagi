//! Memory CRUD operations over the memory store.
//!
//! The application-level operations both the human CLI and the agent-facing MCP
//! tools (`memory_*`) call: save (create or overwrite by name), fetch, list, and
//! delete a durable agent memory. Each takes the injected [`MemoryStore`] and,
//! for [`save`], the current time (`now`), so this layer stays clock-free and
//! testable; the concrete store and clock are bound by the caller.

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::domain::memory::{Memory, MemorySummary, MemoryType, slugify};
use crate::infrastructure::store::memory::MemoryStore;

/// The fields supplied when saving a memory. `name` is slugified into the
/// filename-safe identity by [`save`]; the timestamps are assigned there.
#[derive(Debug, Clone, Default)]
pub struct NewMemory {
    pub name: String,
    pub title: String,
    pub kind: MemoryType,
    pub related: Vec<String>,
    pub body: String,
}

/// Save a memory: slugify its name, then create it or overwrite the existing
/// memory with that name. `created_at` is preserved from an existing memory (so
/// a save is an edit, not a re-creation) or stamped `now` for a new one;
/// `updated_at` is always `now`. Returns the saved memory.
///
/// # Errors
///
/// Returns an error when the store cannot be read or written.
pub fn save(store: &MemoryStore, spec: NewMemory, now: DateTime<Utc>) -> Result<Memory> {
    let name = slugify(&spec.name);
    let lock = store.lock()?;
    let created_at = store
        .read(&name)?
        .map_or(now, |existing| existing.created_at);
    let memory = Memory {
        name,
        title: spec.title,
        kind: spec.kind,
        related: spec.related,
        created_at,
        updated_at: now,
        body: spec.body,
    };
    store.write_locked(&lock, &memory)?;
    Ok(memory)
}

/// Fetch one memory by name, or `None` when it does not exist.
///
/// # Errors
///
/// Returns an error when the name is unsafe or the backing file cannot be read
/// or parsed.
pub fn get(store: &MemoryStore, name: &str) -> Result<Option<Memory>> {
    store.read(name)
}

/// Metadata summaries for every memory, in name order.
///
/// # Errors
///
/// Returns an error when the index cannot be read and the markdown source cannot
/// be rescanned.
pub fn list(store: &MemoryStore) -> Result<Vec<MemorySummary>> {
    store.summaries()
}

/// Delete the memory named `name`, returning whether one was removed.
///
/// # Errors
///
/// Returns an error when the name is unsafe, the lock cannot be taken, or the
/// file cannot be removed.
pub fn delete(store: &MemoryStore, name: &str) -> Result<bool> {
    store.remove(name)
}

#[cfg(test)]
mod tests {
    use super::{NewMemory, delete, get, list, save};
    use crate::domain::memory::MemoryType;
    use crate::infrastructure::store::memory::MemoryStore;
    use chrono::{DateTime, TimeZone, Utc};

    fn ts(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, day, 0, 0, 0).unwrap()
    }

    fn store() -> (tempfile::TempDir, MemoryStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path());
        (tmp, store)
    }

    fn spec(name: &str, title: &str) -> NewMemory {
        NewMemory {
            name: name.to_string(),
            title: title.to_string(),
            kind: MemoryType::User,
            body: "body".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn save_slugifies_the_name_and_stamps_times() {
        let (_tmp, store) = store();
        let saved = save(&store, spec("My Fact!", "A fact"), ts(20)).unwrap();
        assert_eq!(saved.name, "my-fact");
        assert_eq!(saved.kind, MemoryType::User);
        assert_eq!(saved.created_at, ts(20));
        assert_eq!(saved.updated_at, ts(20));
        assert_eq!(get(&store, "my-fact").unwrap().unwrap().title, "A fact");
    }

    #[test]
    fn save_overwrites_by_name_and_preserves_created_at() {
        let (_tmp, store) = store();
        save(&store, spec("fact", "Old"), ts(20)).unwrap();

        let mut edit = spec("fact", "New");
        edit.body = "changed".to_string();
        let saved = save(&store, edit, ts(22)).unwrap();

        // Same identity, edited: created_at kept, updated_at advanced.
        assert_eq!(saved.created_at, ts(20));
        assert_eq!(saved.updated_at, ts(22));
        assert_eq!(list(&store).unwrap().len(), 1);
        assert_eq!(get(&store, "fact").unwrap().unwrap().title, "New");
    }

    #[test]
    fn get_is_none_for_a_missing_memory() {
        let (_tmp, store) = store();
        assert!(get(&store, "nope").unwrap().is_none());
    }

    #[test]
    fn list_returns_summaries_in_name_order() {
        let (_tmp, store) = store();
        save(&store, spec("beta", "B"), ts(20)).unwrap();
        save(&store, spec("alpha", "A"), ts(20)).unwrap();
        let names: Vec<String> = list(&store).unwrap().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn delete_removes_a_saved_memory_and_reports_success() {
        let (_tmp, store) = store();
        save(&store, spec("fact", "A fact"), ts(20)).unwrap();
        assert!(delete(&store, "fact").unwrap());
        assert!(get(&store, "fact").unwrap().is_none());
        assert!(!delete(&store, "fact").unwrap());
    }
}

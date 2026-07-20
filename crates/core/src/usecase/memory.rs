//! Memory CRUD operations over the memory store.
//!
//! The application-level operations both the human CLI and the agent-facing MCP
//! tools (`memory_*`) call: save (create or overwrite by name), fetch, list, and
//! delete a durable agent memory. Each takes the injected [`MemoryStore`] and,
//! for [`save`], the current time (`now`), so this layer stays clock-free and
//! testable; the concrete store and clock are bound by the caller.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::domain::memory::{Memory, MemorySummary, MemoryType, slugify};
use crate::infrastructure::store::memory::MemoryStore;

/// The fields supplied when saving a memory. `name` is slugified into the
/// filename-safe identity by [`save`]; the timestamps are assigned there.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NewMemory {
    pub name: String,
    pub title: String,
    #[serde(default, rename = "type")]
    pub kind: MemoryType,
    #[serde(default)]
    pub related: Vec<String>,
    #[serde(default)]
    pub body: String,
}

/// Optional fields accepted by the `memory_save` upsert surface.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct MemoryPatch {
    pub title: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<MemoryType>,
    pub related: Option<Vec<String>>,
    pub body: Option<String>,
}

/// Filters applied to memory searches.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct MemoryFilter {
    #[serde(rename = "type")]
    pub kind: Option<MemoryType>,
}

/// Save a memory: slugify its name, then create it or overwrite the existing
/// memory with that name. `created_at` is preserved from an existing memory (so
/// a save is an edit, not a re-creation) or stamped `now` for a new one;
/// `updated_at` is always `now`. Returns the saved memory.
///
/// # Errors
///
/// Returns an error when the store cannot be read or written.
#[coverage(off)]
pub fn save(store: &MemoryStore, spec: NewMemory, now: DateTime<Utc>) -> Result<Memory> {
    let name = slugify(&spec.name);
    let lock = store.lock()?;
    let created_at = store
        .read_locked(&name)?
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
#[coverage(off)]
pub fn get(store: &MemoryStore, name: &str) -> Result<Option<Memory>> {
    store.read(name)
}

/// Metadata summaries for every memory, in name order.
///
/// # Errors
///
/// Returns an error when the index cannot be read and the markdown source cannot
/// be rescanned.
#[coverage(off)]
pub fn list(store: &MemoryStore) -> Result<Vec<MemorySummary>> {
    store.summaries()
}

/// Partially update an existing memory or create one. New memories require a
/// title; omitted fields take their domain defaults.
///
/// # Errors
///
/// Returns an error when a new memory has no title or persistence fails.
pub fn save_partial(
    store: &MemoryStore,
    name: &str,
    patch: MemoryPatch,
    now: DateTime<Utc>,
) -> Result<Memory> {
    let slug = slugify(name);
    let lock = store.lock()?;
    let memory = if let Some(mut memory) = store.read_locked(&slug)? {
        if let Some(title) = patch.title {
            memory.title = title;
        }
        if let Some(kind) = patch.kind {
            memory.kind = kind;
        }
        if let Some(related) = patch.related {
            memory.related = related;
        }
        if let Some(body) = patch.body {
            memory.body = body;
        }
        memory.updated_at = now;
        memory
    } else {
        Memory {
            name: slug,
            title: patch
                .title
                .ok_or_else(|| anyhow::anyhow!("`title` is required when creating a new memory"))?,
            kind: patch.kind.unwrap_or_default(),
            related: patch.related.unwrap_or_default(),
            created_at: now,
            updated_at: now,
            body: patch.body.unwrap_or_default(),
        }
    };
    store.write_locked(&lock, &memory)?;
    Ok(memory)
}

/// Search memory names, titles, and bodies case-insensitively, optionally
/// filtering by type. Results are newest first.
///
/// # Errors
///
/// Returns an error when the store cannot be scanned or indexed.
pub fn search(
    store: &MemoryStore,
    query: &str,
    filter: &MemoryFilter,
) -> Result<Vec<MemorySummary>> {
    let needle = query.to_lowercase();
    let mut summaries = if needle.is_empty() {
        store.summaries()?
    } else {
        store
            .scan_lenient()?
            .into_iter()
            .filter(|memory| {
                memory.name.to_lowercase().contains(&needle)
                    || memory.title.to_lowercase().contains(&needle)
                    || memory.body.to_lowercase().contains(&needle)
            })
            .map(|memory| memory.summary())
            .collect()
    };
    summaries.retain(|summary| filter.kind.is_none_or(|kind| summary.kind == kind));
    summaries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.name.cmp(&b.name)));
    Ok(summaries)
}

/// Delete the memory named `name`, returning whether one was removed.
///
/// # Errors
///
/// Returns an error when the name is unsafe, the lock cannot be taken, or the
/// file cannot be removed.
#[coverage(off)]
pub fn delete(store: &MemoryStore, name: &str) -> Result<bool> {
    store.remove(name)
}

#[cfg(test)]
mod tests {
    use super::{
        MemoryFilter, MemoryPatch, NewMemory, delete, get, list, save, save_partial, search,
    };
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

    #[test]
    fn partial_save_creates_with_defaults_and_requires_title() {
        let (_tmp, store) = store();
        let saved = save_partial(
            &store,
            "New Fact!",
            MemoryPatch {
                title: Some("Fact".into()),
                ..Default::default()
            },
            ts(20),
        )
        .unwrap();
        assert_eq!(saved.name, "new-fact");
        assert_eq!(saved.kind, MemoryType::Project);
        assert!(saved.related.is_empty());
        assert!(saved.body.is_empty());

        let error = save_partial(&store, "missing", MemoryPatch::default(), ts(20)).unwrap_err();
        assert!(error.to_string().contains("title"));
    }

    #[test]
    fn partial_save_updates_only_supplied_fields() {
        let (_tmp, store) = store();
        save(&store, spec("fact", "Old"), ts(20)).unwrap();
        let saved = save_partial(
            &store,
            "fact",
            MemoryPatch {
                title: Some("New".into()),
                kind: Some(MemoryType::Feedback),
                related: Some(vec!["other".into()]),
                body: Some("changed".into()),
            },
            ts(22),
        )
        .unwrap();
        assert_eq!(saved.title, "New");
        assert_eq!(saved.kind, MemoryType::Feedback);
        assert_eq!(saved.related, vec!["other"]);
        assert_eq!(saved.body, "changed");
        assert_eq!(saved.created_at, ts(20));
        assert_eq!(saved.updated_at, ts(22));

        let unchanged = save_partial(&store, "fact", MemoryPatch::default(), ts(23)).unwrap();
        assert_eq!(unchanged.title, "New");
        assert_eq!(unchanged.kind, MemoryType::Feedback);
    }

    #[test]
    fn search_matches_text_filters_and_orders_newest_first() {
        let (_tmp, store) = store();
        save(&store, spec("alpha", "Needle title"), ts(20)).unwrap();
        let mut beta = spec("beta", "Beta");
        beta.kind = MemoryType::Feedback;
        beta.body = "needle body".into();
        save(&store, beta, ts(22)).unwrap();

        let all = search(&store, "", &MemoryFilter::default()).unwrap();
        assert_eq!(
            all.iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["beta", "alpha"]
        );
        let filtered = search(
            &store,
            "NEEDLE",
            &MemoryFilter {
                kind: Some(MemoryType::Feedback),
            },
        )
        .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "beta");
        assert!(
            search(&store, "absent", &MemoryFilter::default())
                .unwrap()
                .is_empty()
        );
    }
}

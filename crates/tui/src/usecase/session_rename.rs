//! User-facing session names; checkout names and daemon identities stay stable.

use chrono::{DateTime, Utc};
use usagi_core::domain::id::SessionId;
use usagi_core::domain::presentation_text::presentation_text_is_safe;
use usagi_core::usecase::ports::WorkspaceStateRepository;

/// Normalize a single-line display name before editing persistent metadata.
///
/// # Errors
/// Returns a display-safe explanation for an empty, unsafe, or oversized name.
pub fn validate(value: &str) -> Result<&str, &'static str> {
    if !presentation_text_is_safe(value) {
        return Err("Use a single line without control characters.");
    }
    let value = value.trim();
    if value.is_empty() {
        return Err("Enter a display name.");
    }
    if value.chars().count() > 64 {
        return Err("Use at most 64 characters.");
    }
    Ok(value)
}

/// Persist a name for a daemon-validated session incarnation under the store lock.
///
/// # Errors
/// Returns an error on invalid input or a failed read, lock, or atomic write.
pub fn save(
    store: &impl WorkspaceStateRepository,
    session: SessionId,
    name: &str,
    now: DateTime<Utc>,
) -> Result<(), &'static str> {
    let name = validate(name)?;
    store
        .transact(|transaction| {
            let mut state = transaction.load()?.unwrap_or_default();
            state.session_display_names.insert(session, name.to_owned());
            state.updated_at = now;
            transaction.save(&state)
        })
        .map_err(|_| "Session name could not be saved.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use usagi_core::infrastructure::store::state::WorkspaceStateStore;

    #[test]
    fn validation_and_persistence_preserve_other_metadata_and_incarnations() {
        for value in [
            "",
            "  ",
            "bad\nname",
            "bad\u{1b}name",
            "bad\u{202e}name",
            &"a".repeat(65),
        ] {
            assert!(validate(value).is_err());
        }
        assert_eq!(validate(&"あ".repeat(64)).unwrap().chars().count(), 64);
        let directory = tempfile::tempdir().unwrap();
        let store = WorkspaceStateStore::new(directory.path());
        let first = SessionId::new();
        let second = SessionId::new();
        save(&store, first, "  名前 👩‍💻  ", Utc::now()).unwrap();
        let mut state = store.load().unwrap().unwrap();
        state.root_notes.note = Some("keep".into());
        store.save(&state).unwrap();
        save(&store, second, "second", Utc::now()).unwrap();
        save(&store, first, "renamed", Utc::now()).unwrap();
        let restored = WorkspaceStateStore::new(directory.path())
            .load()
            .unwrap()
            .unwrap();
        assert_eq!(restored.session_display_names[&first], "renamed");
        assert_eq!(restored.session_display_names[&second], "second");
        assert!(
            !restored
                .session_display_names
                .contains_key(&SessionId::new())
        );
        assert_eq!(restored.root_notes.note.as_deref(), Some("keep"));
        assert_eq!(restored.sessions, Vec::new());
        assert!(save(&store, first, "\n", Utc::now()).is_err());
        assert_eq!(store.load().unwrap().unwrap(), restored);
        std::fs::write(store.state_path(), "broken").unwrap();
        assert!(save(&store, first, "retry", Utc::now()).is_err());
        assert_eq!(
            std::fs::read_to_string(store.state_path()).unwrap(),
            "broken"
        );
    }
}

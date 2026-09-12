//! The session scratchpad actions: note, todo and decision log.
//!
//! These live beside the dispatch table rather than inside it because they share
//! one shape — read or write one machine-local store in the caller's own session
//! worktree — and because the table itself has a line budget the rest of the
//! daemon has to fit inside (`tests/architecture.rs`).

use usagi_core::infrastructure::client::SessionAction;
use usagi_core::infrastructure::store::state::WorkspaceStateStore;
use usagi_core::usecase::note;
use usagi_daemon::usecase::session_runtime::SessionRuntimeError;

/// Apply one scratchpad action to the store in `path` and answer with the part
/// of the scratchpad it concerns.
///
/// The caller has already resolved `path` from its own credential, so this
/// function never chooses a session.
///
/// # Errors
/// Returns `InvalidRequest` for an action this module does not own and for a
/// missing or empty field, and `Storage` for a store failure.
pub(super) fn read_or_write(
    action: SessionAction,
    payload: &serde_json::Value,
    path: &std::path::Path,
) -> Result<serde_json::Value, SessionRuntimeError> {
    let store = WorkspaceStateStore::new(path);
    let target = note::Target::Root;
    let string = |key: &str| {
        payload
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or(SessionRuntimeError::InvalidRequest)
    };
    Ok(match action {
        SessionAction::NoteGet => {
            serde_json::json!({"note": note::note(&store, target).map_err(|_| SessionRuntimeError::Storage)?})
        }
        SessionAction::NoteUpdate => {
            let value = payload
                .get("note")
                .and_then(serde_json::Value::as_str)
                .ok_or(SessionRuntimeError::InvalidRequest)?;
            note::set_note(&store, target, value, chrono::Utc::now())
                .map_err(|_| SessionRuntimeError::Storage)?;
            serde_json::json!({"note": note::note(&store, target).map_err(|_| SessionRuntimeError::Storage)?})
        }
        SessionAction::TodoList => {
            serde_json::json!({"todos": note::todos(&store, target).map_err(|_| SessionRuntimeError::Storage)?})
        }
        SessionAction::TodoAdd => {
            let text = string("text")?;
            note::add_todo(&store, target, text, chrono::Utc::now())
                .map_err(|_| SessionRuntimeError::Storage)?;
            serde_json::json!({"todos": note::todos(&store, target).map_err(|_| SessionRuntimeError::Storage)?})
        }
        SessionAction::TodoUpdate => {
            let index = payload
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or(SessionRuntimeError::InvalidRequest)?;
            let done = payload
                .get("done")
                .map(|value| value.as_bool().ok_or(SessionRuntimeError::InvalidRequest))
                .transpose()?;
            let text = payload
                .get("text")
                .map(|value| {
                    value
                        .as_str()
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(str::to_owned)
                        .ok_or(SessionRuntimeError::InvalidRequest)
                })
                .transpose()?;
            if done.is_none() && text.is_none() {
                return Err(SessionRuntimeError::InvalidRequest);
            }
            if !note::update_todo(&store, target, index, done, text, chrono::Utc::now())
                .map_err(|_| SessionRuntimeError::Storage)?
            {
                return Err(SessionRuntimeError::InvalidRequest);
            }
            serde_json::json!({"todos": note::todos(&store, target).map_err(|_| SessionRuntimeError::Storage)?})
        }
        SessionAction::TodoRemove => {
            let index = payload
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or(SessionRuntimeError::InvalidRequest)?;
            if !note::remove_todo(&store, target, index, chrono::Utc::now())
                .map_err(|_| SessionRuntimeError::Storage)?
            {
                return Err(SessionRuntimeError::InvalidRequest);
            }
            serde_json::json!({"todos": note::todos(&store, target).map_err(|_| SessionRuntimeError::Storage)?})
        }
        SessionAction::DecisionList => {
            serde_json::json!({"decisions": note::decisions(&store, target).map_err(|_| SessionRuntimeError::Storage)?})
        }
        SessionAction::DecisionLog => {
            let text = string("text")?;
            note::log_decision(&store, target, text, chrono::Utc::now())
                .map_err(|_| SessionRuntimeError::Storage)?;
            serde_json::json!({"decisions": note::decisions(&store, target).map_err(|_| SessionRuntimeError::Storage)?})
        }
        // Reaching here means the caller passed an action this module does not
        // own. It was impossible while this lived inside the dispatch table's
        // match; as its own item it is an ordinary caller error.
        _ => return Err(SessionRuntimeError::InvalidRequest),
    })
}

#[cfg(test)]
mod tests {
    use super::{SessionAction, SessionRuntimeError, read_or_write};
    use serde_json::json;

    fn call(
        action: SessionAction,
        payload: &serde_json::Value,
        path: &std::path::Path,
    ) -> Result<serde_json::Value, SessionRuntimeError> {
        read_or_write(action, payload, path)
    }

    #[test]
    fn the_scratchpad_reads_and_writes_one_session_worktree() {
        let worktree = tempfile::tempdir().unwrap();
        let path = worktree.path();

        // An unwritten note reads as absent, and an update answers with what it
        // wrote.
        assert_eq!(
            call(SessionAction::NoteGet, &json!({}), path).unwrap()["note"],
            json!(null)
        );
        assert_eq!(
            call(
                SessionAction::NoteUpdate,
                &json!({"note": "why this branch"}),
                path
            )
            .unwrap()["note"],
            json!("why this branch")
        );
        assert_eq!(
            call(SessionAction::NoteGet, &json!({}), path).unwrap()["note"],
            json!("why this branch")
        );
        // A note may be cleared; every other field is required.
        assert!(call(SessionAction::NoteUpdate, &json!({"note": ""}), path).is_ok());
        assert!(call(SessionAction::NoteUpdate, &json!({}), path).is_err());

        // Todos: add, then mark done, then remove.
        assert_eq!(
            call(SessionAction::TodoList, &json!({}), path).unwrap()["todos"],
            json!([])
        );
        let added = call(
            SessionAction::TodoAdd,
            &json!({"text": " write the test "}),
            path,
        )
        .unwrap();
        assert_eq!(added["todos"][0]["text"], json!("write the test"));
        // An unchecked todo omits `done` on the wire (the domain skips the
        // default), so its absence is what "not done" looks like here.
        assert_eq!(added["todos"][0]["done"], json!(null));
        assert!(call(SessionAction::TodoAdd, &json!({"text": "  "}), path).is_err());

        let updated = call(
            SessionAction::TodoUpdate,
            &json!({"index": 0, "done": true}),
            path,
        )
        .unwrap();
        assert_eq!(updated["todos"][0]["done"], json!(true));
        // An update has to change something, name a real entry, and use the
        // declared types.
        assert!(call(SessionAction::TodoUpdate, &json!({"index": 0}), path).is_err());
        assert!(call(SessionAction::TodoUpdate, &json!({"done": true}), path).is_err());
        assert!(
            call(
                SessionAction::TodoUpdate,
                &json!({"index": 0, "done": "yes"}),
                path
            )
            .is_err()
        );
        assert!(
            call(
                SessionAction::TodoUpdate,
                &json!({"index": 0, "text": ""}),
                path
            )
            .is_err()
        );
        assert!(
            call(
                SessionAction::TodoUpdate,
                &json!({"index": 7, "done": true}),
                path
            )
            .is_err()
        );

        assert_eq!(
            call(SessionAction::TodoRemove, &json!({"index": 0}), path).unwrap()["todos"],
            json!([])
        );
        assert!(call(SessionAction::TodoRemove, &json!({"index": 0}), path).is_err());
        assert!(call(SessionAction::TodoRemove, &json!({}), path).is_err());

        // Decisions append and are read back.
        assert_eq!(
            call(SessionAction::DecisionList, &json!({}), path).unwrap()["decisions"],
            json!([])
        );
        let logged = call(
            SessionAction::DecisionLog,
            &json!({"text": "split the table"}),
            path,
        )
        .unwrap();
        assert_eq!(logged["decisions"][0]["text"], json!("split the table"));
        assert!(call(SessionAction::DecisionLog, &json!({}), path).is_err());

        // An action this module does not own is refused rather than panicking:
        // as its own item the function is reachable from anywhere in the crate.
        assert!(matches!(
            call(SessionAction::Create, &json!({}), path),
            Err(SessionRuntimeError::InvalidRequest)
        ));
    }
}

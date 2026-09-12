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
/// Returns `InvalidRequest` for a missing or empty field and `Storage` for a
/// store failure.
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
        _ => unreachable!(),
    })
}

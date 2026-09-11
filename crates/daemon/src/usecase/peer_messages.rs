//! Authenticated, current-session peer communication, independent of dispatch completion.

use serde::Deserialize;
use serde_json::{Value, json};
use usagi_core::domain::agent::CallerRef;
use usagi_core::domain::agent_message::SendMessage;
use usagi_core::domain::id::{OperationId, WorkspaceId};
use usagi_core::infrastructure::client::DispatchToolAction;
use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};
use usagi_core::infrastructure::store::dispatch::DispatchStore;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Handoff {
    agent: Value,
    prompt: String,
}

/// Resolve a handoff exclusively from authenticated session identity, retaining
/// its existing role for the ordinary delegation policy check.
///
/// # Errors
/// Rejects root scope, unavailable sessions and malformed or oversized input.
pub fn handoff_payload(
    caller: &CallerRef,
    snapshot: &Value,
    payload: Value,
) -> Result<Value, ProtocolError> {
    let input: Handoff = serde_json::from_value(payload).map_err(|_| invalid())?;
    if input.prompt.trim().is_empty()
        || input.prompt.len() > 16 * 1024
        || input.prompt.contains('\0')
    {
        return Err(invalid());
    }
    let id = caller.session_id.ok_or_else(|| {
        ProtocolError::new(
            ErrorCode::PermissionDenied,
            "handoff requires a managed session",
        )
    })?;
    let session = snapshot
        .get("sessions")
        .and_then(Value::as_array)
        .and_then(|sessions| {
            sessions
                .iter()
                .find(|session| session.get("session_id") == Some(&json!(id)))
        })
        .ok_or_else(|| unavailable(anyhow::anyhow!("session unavailable")))?;
    let name = session
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    Ok(
        json!({"session": {"name":name, "role":session.get("role_id")}, "agent":input.agent, "prompt":input.prompt}),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Query {
    #[serde(default)]
    after: Option<OperationId>,
    #[serde(default = "page_limit")]
    limit: usize,
    #[serde(default)]
    unread_only: bool,
}
fn page_limit() -> usize {
    100
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ack {
    message_id: OperationId,
}

fn invalid() -> ProtocolError {
    ProtocolError::new(ErrorCode::InvalidArgument, "invalid peer message request")
}
fn unavailable(_: anyhow::Error) -> ProtocolError {
    ProtocolError::new(
        ErrorCode::Unavailable,
        "peer message request could not be committed or read; check participants, reply, identity and capacity",
    )
}

/// Handle a peer operation after daemon credential and workspace verification.
///
/// # Errors
/// Rejects root scope, foreign membership, malformed requests and persistence failures.
pub fn handle(
    store: &DispatchStore,
    workspace: WorkspaceId,
    caller: &CallerRef,
    run: OperationId,
    action: DispatchToolAction,
    payload: Value,
) -> Result<Value, ProtocolError> {
    let session = caller.session_id.ok_or_else(|| {
        ProtocolError::new(
            ErrorCode::PermissionDenied,
            "peer operations require a managed session",
        )
    })?;
    let participant = store
        .agent_in_workspace(workspace, caller.agent_id)
        .map_err(unavailable)?;
    if participant.is_none_or(|agent| agent.session_id != Some(session)) {
        return Err(ProtocolError::new(
            ErrorCode::PermissionDenied,
            "caller is not a session participant",
        ));
    }
    match action {
        DispatchToolAction::AgentPeers => {
            if payload.as_object().is_none_or(|object| !object.is_empty()) {
                return Err(invalid());
            }
            let peers: Vec<_> = store.agents_in_workspace(workspace).map_err(unavailable)?.into_iter().filter(|agent| agent.session_id == Some(session)).map(|agent| json!({"agent_id": agent.agent_id, "runtime": agent.runtime, "model": agent.model, "status": agent.status})).collect();
            Ok(json!({"self_agent_id": caller.agent_id, "agents": peers}))
        }
        DispatchToolAction::AgentMessage => {
            let message: SendMessage = serde_json::from_value(payload).map_err(|_| invalid())?;
            if !message.is_valid() {
                return Err(invalid());
            }
            Ok(
                json!({"message": store.send_message(workspace, caller, run, message).map_err(unavailable)?}),
            )
        }
        DispatchToolAction::AgentMessages => {
            let query: Query = serde_json::from_value(payload).map_err(|_| invalid())?;
            if !(1..=100).contains(&query.limit) {
                return Err(invalid());
            }
            let messages = store
                .messages(
                    workspace,
                    caller,
                    query.after,
                    query.limit,
                    query.unread_only,
                )
                .map_err(unavailable)?;
            Ok(
                json!({"next_cursor": messages.last().map(|entry| entry.message.message_id), "messages": messages}),
            )
        }
        DispatchToolAction::AgentMessageAck => {
            let ack: Ack = serde_json::from_value(payload).map_err(|_| invalid())?;
            store
                .acknowledge_message(workspace, caller, ack.message_id)
                .map_err(unavailable)?;
            Ok(json!({"acknowledged": ack.message_id}))
        }
        _ => Err(invalid()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use usagi_core::domain::agent::{Agent, AgentProfileId, AgentStatus, ModelSelector};
    use usagi_core::domain::id::{AgentId, SessionId};

    fn participant(store: &DispatchStore, workspace: WorkspaceId, session: SessionId) -> CallerRef {
        let agent_id = AgentId::new();
        store
            .upsert_agent(
                workspace,
                Agent {
                    agent_id,
                    session_id: Some(session),
                    runtime: AgentProfileId::new("claude").unwrap(),
                    model: ModelSelector::new("default").unwrap(),
                    status: AgentStatus::Idle,
                    current_run: None,
                },
            )
            .unwrap();
        CallerRef {
            session_id: Some(session),
            agent_id,
        }
    }

    #[test]
    fn authenticated_peer_tools_roundtrip_and_validate_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(dir.path());
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let sender = participant(&store, workspace, session);
        let receiver = participant(&store, workspace, session);
        let foreign = participant(&store, workspace, SessionId::new());
        let run = OperationId::new();
        let call = |caller: &CallerRef, action, payload| {
            handle(&store, workspace, caller, run, action, payload)
        };
        let peers = call(&sender, DispatchToolAction::AgentPeers, json!({})).unwrap();
        assert_eq!(peers["agents"].as_array().unwrap().len(), 2);
        let id = OperationId::new();
        let sent = call(&sender,DispatchToolAction::AgentMessage,json!({"message_id":id,"to_agent_id":receiver.agent_id,"kind":"message","body":"Review please"})).unwrap();
        assert_eq!(sent["message"]["from_agent_id"], json!(sender.agent_id));
        let inbox = call(
            &receiver,
            DispatchToolAction::AgentMessages,
            json!({"unread_only":true}),
        )
        .unwrap();
        assert_eq!(inbox["next_cursor"], json!(id));
        call(
            &receiver,
            DispatchToolAction::AgentMessageAck,
            json!({"message_id":id}),
        )
        .unwrap();
        assert_eq!(
            call(
                &receiver,
                DispatchToolAction::AgentMessages,
                json!({"unread_only":true})
            )
            .unwrap()["messages"],
            json!([])
        );
        assert_eq!(
            call(&foreign, DispatchToolAction::AgentMessages, json!({})).unwrap()["messages"],
            json!([])
        );
        for (action, payload) in [
            (DispatchToolAction::AgentPeers, json!({"session":"foreign"})),
            (DispatchToolAction::AgentPeers, json!(null)),
            (DispatchToolAction::AgentMessage, json!({})),
            (
                DispatchToolAction::AgentMessage,
                json!({"message_id":OperationId::new(),"to_agent_id":receiver.agent_id,"kind":"message","body":" "}),
            ),
            (DispatchToolAction::AgentMessages, json!({"limit":"bad"})),
            (DispatchToolAction::AgentMessages, json!({"limit":0})),
            (DispatchToolAction::AgentMessages, json!({"limit":101})),
            (DispatchToolAction::AgentMessageAck, json!({})),
            (DispatchToolAction::AgentGet, json!({})),
        ] {
            assert_eq!(
                call(&sender, action, payload).unwrap_err().code,
                ErrorCode::InvalidArgument
            );
        }
        assert_eq!(
            call(
                &sender,
                DispatchToolAction::AgentMessageAck,
                json!({"message_id":id})
            )
            .unwrap_err()
            .code,
            ErrorCode::Unavailable
        );
        for caller in [
            CallerRef {
                session_id: None,
                agent_id: sender.agent_id,
            },
            CallerRef {
                session_id: Some(session),
                agent_id: foreign.agent_id,
            },
            CallerRef {
                session_id: Some(session),
                agent_id: AgentId::new(),
            },
        ] {
            assert_eq!(
                call(&caller, DispatchToolAction::AgentPeers, json!({}))
                    .unwrap_err()
                    .code,
                ErrorCode::PermissionDenied
            );
        }
    }

    #[test]
    fn handoff_cannot_select_a_session_or_change_its_role() {
        let caller = CallerRef {
            session_id: Some(SessionId::new()),
            agent_id: AgentId::new(),
        };
        let input = json!({"agent":{"runtime":"claude","model":"default"},"prompt":"review"});
        let snapshot = json!({"sessions":[{"session_id":caller.session_id,"name":"current","role_id":"implementer"}]});
        let resolved = handoff_payload(&caller, &snapshot, input.clone()).unwrap();
        assert_eq!(
            resolved["session"],
            json!({"name":"current","role":"implementer"})
        );
        assert!(handoff_payload(&caller, &json!({}), input.clone()).is_err());
        assert!(
            handoff_payload(
                &CallerRef {
                    session_id: None,
                    ..caller.clone()
                },
                &snapshot,
                input.clone()
            )
            .is_err()
        );
        for prompt in [" ".to_owned(), "\0".to_owned(), "a".repeat(16385)] {
            let mut invalid = input.clone();
            invalid["prompt"] = json!(prompt);
            assert!(handoff_payload(&caller, &snapshot, invalid).is_err());
        }
        let mut invalid = input.clone();
        invalid["session"] = json!("foreign");
        assert!(handoff_payload(&caller, &snapshot, invalid).is_err());
        assert!(
            handoff_payload(
                &caller,
                &json!({"sessions":[{"session_id":caller.session_id}]}),
                input
            )
            .is_err()
        );
    }
}

//! A separate journal prevents peer messages from being consumed as run completion.

use anyhow::{Context, Result, ensure};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use super::DispatchStore;
use crate::domain::agent::CallerRef;
use crate::domain::agent_message::{AgentMessage, MessageKind, SendMessage};
use crate::domain::id::{AgentId, OperationId, SessionId, WorkspaceId};
use crate::infrastructure::persistence::{json_file, store_lock::StoreLock};

const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_MESSAGES: usize = 4096;

#[derive(Serialize, Deserialize)]
struct Messages {
    version: u32,
    entries: Vec<AgentMessage>,
}

impl Default for Messages {
    fn default() -> Self {
        Self {
            version: 1,
            entries: Vec::new(),
        }
    }
}

fn read_messages(path: &std::path::Path) -> Result<Messages> {
    let data: Messages = json_file::read_bounded(path, MAX_BYTES)?.unwrap_or_default();
    ensure!(data.version == 1, "unsupported peer journal version");
    Ok(data)
}

impl DispatchStore {
    fn message_path(&self, workspace: WorkspaceId, session: SessionId) -> std::path::PathBuf {
        self.dir
            .join("peer-messages")
            .join(workspace.as_str())
            .join(format!("{}.json", session.as_str()))
    }

    fn require_message_participant(
        &self,
        workspace: WorkspaceId,
        session: SessionId,
        agent: AgentId,
    ) -> Result<()> {
        ensure!(
            self.agent_in_workspace(workspace, agent)?
                .is_some_and(|item| item.session_id == Some(session)),
            "agent is not a session participant"
        );
        Ok(())
    }

    /// Append once after checking both participants and any reply correlation.
    ///
    /// # Errors
    /// Rejects invalid payloads, foreign participants, conflicting retries and capacity exhaustion.
    pub fn send_message(
        &self,
        workspace: WorkspaceId,
        caller: &CallerRef,
        run: OperationId,
        message: SendMessage,
    ) -> Result<AgentMessage> {
        ensure!(message.is_valid(), "invalid peer message");
        let session = caller
            .session_id
            .context("peer messages require a managed session")?;
        ensure!(
            caller.agent_id != message.to_agent_id,
            "peer message requires another agent"
        );
        let _lock = StoreLock::acquire(&self.dir)?;
        self.require_message_participant(workspace, session, caller.agent_id)?;
        self.require_message_participant(workspace, session, message.to_agent_id)?;
        let path = self.message_path(workspace, session);
        let mut data = read_messages(&path)?;
        if let Some(existing) = data
            .entries
            .iter()
            .find(|entry| entry.message.message_id == message.message_id)
        {
            ensure!(
                existing.from_agent_id == caller.agent_id && existing.message == message,
                "peer message identity conflict"
            );
            return Ok(existing.clone());
        }
        if let Some(reply) = message.in_reply_to {
            let original = data
                .entries
                .iter()
                .find(|entry| entry.message.message_id == reply)
                .context("reply target was not found")?;
            ensure!(
                original.from_agent_id == message.to_agent_id
                    && original.message.to_agent_id == caller.agent_id,
                "reply participants do not match"
            );
            if matches!(
                message.kind,
                MessageKind::Approved | MessageKind::ChangesRequested
            ) {
                ensure!(
                    original.message.kind == MessageKind::ReviewRequest
                        && original.message.review == message.review,
                    "review target does not match request"
                );
                ensure!(
                    !data
                        .entries
                        .iter()
                        .any(|entry| entry.message.in_reply_to == Some(reply)
                            && matches!(
                                entry.message.kind,
                                MessageKind::Approved | MessageKind::ChangesRequested
                            )),
                    "review already has a verdict"
                );
            }
        }
        ensure!(
            data.entries.len() < MAX_MESSAGES,
            "peer message capacity exhausted"
        );
        let entry = AgentMessage {
            from_agent_id: caller.agent_id,
            from_run_id: run,
            message,
            created_at: Utc::now(),
            acknowledged: false,
        };
        data.entries.push(entry.clone());
        ensure!(
            serde_json::to_vec_pretty(&data)?.len() < MAX_BYTES,
            "peer message byte capacity exhausted"
        );
        json_file::write_atomic(
            path.parent().context("message parent unavailable")?,
            &path,
            &data,
        )?;
        Ok(entry)
    }

    /// Read messages in which the authenticated Agent is a participant.
    ///
    /// # Errors
    /// Rejects missing membership, invalid bounds, missing cursors or unreadable storage.
    pub fn messages(
        &self,
        workspace: WorkspaceId,
        caller: &CallerRef,
        after: Option<OperationId>,
        limit: usize,
        unread_only: bool,
    ) -> Result<Vec<AgentMessage>> {
        ensure!((1..=100).contains(&limit), "invalid message page limit");
        let session = caller
            .session_id
            .context("peer messages require a managed session")?;
        self.require_message_participant(workspace, session, caller.agent_id)?;
        let data = read_messages(&self.message_path(workspace, session))?;
        let visible: Vec<_> = data
            .entries
            .into_iter()
            .filter(|entry| {
                entry.from_agent_id == caller.agent_id
                    || entry.message.to_agent_id == caller.agent_id
            })
            .collect();
        let offset = match after {
            Some(id) => {
                visible
                    .iter()
                    .position(|entry| entry.message.message_id == id)
                    .context("message cursor unavailable")?
                    + 1
            }
            None => 0,
        };
        Ok(visible
            .into_iter()
            .skip(offset)
            .filter(|entry| {
                !unread_only
                    || (entry.message.to_agent_id == caller.agent_id && !entry.acknowledged)
            })
            .take(limit)
            .collect())
    }

    /// Acknowledge one received message without completing its dispatch run.
    ///
    /// # Errors
    /// Rejects foreign messages and storage failures.
    pub fn acknowledge_message(
        &self,
        workspace: WorkspaceId,
        caller: &CallerRef,
        id: OperationId,
    ) -> Result<()> {
        let session = caller
            .session_id
            .context("peer messages require a managed session")?;
        let _lock = StoreLock::acquire(&self.dir)?;
        self.require_message_participant(workspace, session, caller.agent_id)?;
        let path = self.message_path(workspace, session);
        let mut data = read_messages(&path)?;
        let entry = data
            .entries
            .iter_mut()
            .find(|entry| {
                entry.message.message_id == id && entry.message.to_agent_id == caller.agent_id
            })
            .context("received message was not found")?;
        entry.acknowledged = true;
        json_file::write_atomic(
            path.parent().context("message parent unavailable")?,
            &path,
            &data,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::{Agent, AgentProfileId, AgentStatus, ModelSelector};
    use crate::domain::agent_message::ReviewTarget;

    fn participant(
        store: &DispatchStore,
        workspace: WorkspaceId,
        session: SessionId,
        runtime: &str,
    ) -> CallerRef {
        let agent_id = AgentId::new();
        store
            .upsert_agent(
                workspace,
                Agent {
                    agent_id,
                    session_id: Some(session),
                    runtime: AgentProfileId::new(runtime).unwrap(),
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

    fn message(to: &CallerRef) -> SendMessage {
        SendMessage {
            message_id: OperationId::new(),
            to_agent_id: to.agent_id,
            kind: MessageKind::Message,
            body: "Please check the error path".into(),
            in_reply_to: None,
            review: None,
        }
    }

    #[test]
    fn cross_runtime_review_replies_survive_restart_and_do_not_complete_runs() {
        let dir = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(dir.path());
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let codex = participant(&store, workspace, session, "codex");
        let claude = participant(&store, workspace, session, "claude");
        let run = OperationId::new();
        let mut request = message(&claude);
        request.kind = MessageKind::ReviewRequest;
        request.review = Some(ReviewTarget {
            base_sha: "a".repeat(40),
            head_sha: "b".repeat(40),
        });
        let saved = store
            .send_message(workspace, &codex, run, request.clone())
            .unwrap();
        assert_eq!(
            store
                .send_message(workspace, &codex, run, request.clone())
                .unwrap(),
            saved
        );
        let reopened = DispatchStore::new(dir.path());
        assert_eq!(
            reopened
                .messages(workspace, &claude, None, 100, true)
                .unwrap(),
            vec![saved.clone()]
        );
        assert!(
            reopened
                .messages(workspace, &codex, None, 100, true)
                .unwrap()
                .is_empty()
        );
        reopened
            .acknowledge_message(workspace, &claude, request.message_id)
            .unwrap();
        reopened
            .acknowledge_message(workspace, &claude, request.message_id)
            .unwrap();
        assert!(
            reopened
                .messages(workspace, &claude, None, 100, true)
                .unwrap()
                .is_empty()
        );
        let mut verdict = message(&codex);
        verdict.kind = MessageKind::ChangesRequested;
        verdict.review = request.review.clone();
        verdict.in_reply_to = Some(request.message_id);
        reopened
            .send_message(workspace, &claude, OperationId::new(), verdict.clone())
            .unwrap();
        assert_eq!(
            reopened
                .messages(workspace, &codex, Some(request.message_id), 1, false)
                .unwrap()[0]
                .message,
            verdict
        );
        assert!(reopened.inbox(&codex).unwrap().is_empty());
        assert!(reopened.runs().unwrap().is_empty());
        verdict.message_id = OperationId::new();
        verdict.kind = MessageKind::Approved;
        assert!(
            reopened
                .send_message(workspace, &claude, run, verdict)
                .is_err()
        );
        assert_ne!(saved.message.message_id, OperationId::new());
    }

    #[test]
    fn message_fences_reject_foreign_participants_and_conflicting_replies() {
        let dir = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(dir.path());
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let a = participant(&store, workspace, session, "codex");
        let b = participant(&store, workspace, session, "claude");
        let foreign = participant(&store, workspace, SessionId::new(), "claude");
        let outsider = participant(&store, WorkspaceId::new(), session, "claude");
        let run = OperationId::new();
        for target in [&a, &foreign, &outsider] {
            assert!(
                store
                    .send_message(workspace, &a, run, message(target))
                    .is_err()
            );
        }
        let request = message(&b);
        store
            .send_message(workspace, &a, run, request.clone())
            .unwrap();
        let mut conflict = request.clone();
        conflict.body = "changed".into();
        assert!(store.send_message(workspace, &a, run, conflict).is_err());
        assert!(
            store
                .acknowledge_message(workspace, &a, request.message_id)
                .is_err()
        );
        assert!(store.messages(workspace, &a, None, 0, false).is_err());
        assert!(
            store
                .messages(workspace, &a, Some(OperationId::new()), 1, false)
                .is_err()
        );
        assert!(
            store
                .messages(workspace, &outsider, None, 1, false)
                .is_err()
        );
        let mut reply = message(&a);
        reply.in_reply_to = Some(OperationId::new());
        assert!(
            store
                .send_message(workspace, &b, run, reply.clone())
                .is_err()
        );
        reply.in_reply_to = Some(request.message_id);
        store
            .send_message(workspace, &b, run, reply.clone())
            .unwrap();
        reply.message_id = OperationId::new();
        reply.kind = MessageKind::Approved;
        reply.review = Some(ReviewTarget {
            base_sha: "a".repeat(40),
            head_sha: "b".repeat(40),
        });
        assert!(store.send_message(workspace, &b, run, reply).is_err());
    }
}

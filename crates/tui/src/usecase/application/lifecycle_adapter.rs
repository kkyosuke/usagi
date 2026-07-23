//! daemon の SessionLifecycle wire を TUI reducer へ接続する adapter。
//!
//! この module は daemon reducer/API を再実装しない。request は producer が発行した
//! [`OperationId`] を変えずに送信し、reconnect は daemon 側で atomic に取得した
//! operation list / subscribe replay / workspace snapshot を適用するだけである。

use std::collections::{HashMap, HashSet};

use usagi_core::domain::id::{OperationId, WorkspaceId};
use usagi_core::usecase::client::ClientError;

use super::lifecycle::{
    DaemonEvent, Effect, Event, LifecycleState, PendingRow, SessionRow, update,
};

/// durable operation の accepted response。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationAccepted {
    pub operation_id: OperationId,
    pub revision: u64,
}

/// subscribe replay の順序軸。operation revision と混同しない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationCursor {
    pub operation_id: OperationId,
    pub after_sequence: u64,
}

/// reconnect のために daemon へ渡す既知 operation と replay cursor。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileRequest {
    pub workspace: WorkspaceId,
    pub operations: Vec<OperationCursor>,
}

/// replay / subscription が返す event。`sequence` は connection 固有でなく、
/// operation stream の monotonic cursor である。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequencedOperationEvent {
    pub operation_id: OperationId,
    pub sequence: u64,
    pub event: OperationEvent,
}

/// daemon operation journal の TUI に必要な安全な projection。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationEvent {
    Accepted {
        row: PendingRow,
    },
    Progress {
        revision: u64,
        message: String,
    },
    Succeeded {
        revision: u64,
        created: Option<SessionRow>,
    },
    Failed {
        revision: u64,
        message: String,
    },
}

/// operation list / subscribe / workspace snapshot を同じ daemon barrier から集めた値。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileSnapshot {
    pub sessions: Vec<SessionRow>,
    pub events: Vec<SequencedOperationEvent>,
}

/// TUI が必要とする daemon control surface。
///
/// `reconcile` は operation list、各 operation の subscribe/replay、workspace
/// snapshot を daemon 側の subscribe barrier で取得する。TUI は reconnect のために
/// mutation を再送しない。
pub trait SessionLifecycleClient {
    /// effect を durable operation として送る。
    ///
    /// # Errors
    ///
    /// daemon が request を送信または受理できない場合に返す。
    fn submit(&mut self, effect: &Effect) -> Result<OperationAccepted, ClientError>;

    /// reconnect した client の state を daemon 正本から再構成する。
    ///
    /// # Errors
    ///
    /// daemon が snapshot/replay barrier を返せない場合に返す。
    fn reconcile(&mut self, request: ReconcileRequest) -> Result<ReconcileSnapshot, ClientError>;
}

/// daemon client と reducer の間で operation identity/cursor を保持する adapter。
#[derive(Debug)]
pub struct SessionLifecycleAdapter<C> {
    client: C,
    intents: HashMap<OperationId, PendingRow>,
    submitted: HashSet<OperationId>,
    sequences: HashMap<OperationId, u64>,
}

impl<C> SessionLifecycleAdapter<C> {
    #[must_use]
    pub fn new(client: C) -> Self {
        Self {
            client,
            intents: HashMap::new(),
            submitted: HashSet::new(),
            sequences: HashMap::new(),
        }
    }

    #[must_use]
    pub fn client(&self) -> &C {
        &self.client
    }

    pub fn client_mut(&mut self) -> &mut C {
        &mut self.client
    }
}

impl<C: SessionLifecycleClient> SessionLifecycleAdapter<C> {
    /// Submit reducer effects once and apply their accepted responses. A
    /// transport failure deliberately leaves the durable intent recorded: a
    /// later reconnect must reconcile it rather than create a new operation.
    ///
    /// # Errors
    ///
    /// daemon が effect を送信または受理できない場合に返す。
    pub fn dispatch(
        &mut self,
        state: &mut LifecycleState,
        effects: impl IntoIterator<Item = Effect>,
    ) -> Result<(), ClientError> {
        for effect in effects {
            let (operation_id, row) = pending_row(state, &effect);
            self.intents.entry(operation_id).or_insert(row);
            if !self.submitted.insert(operation_id) {
                continue;
            }
            let accepted = self.client.submit(&effect)?;
            if accepted.operation_id != operation_id {
                // A response for another durable operation cannot safely be
                // attached to this pending row. Keep the submitted marker so
                // reconnect, rather than blind retry, resolves the intent.
                continue;
            }
            self.apply(
                state,
                SequencedOperationEvent {
                    operation_id,
                    sequence: accepted.revision,
                    event: OperationEvent::Accepted {
                        row: self.intents[&operation_id].clone(),
                    },
                },
            );
        }
        Ok(())
    }

    /// Reconcile after reconnect without replaying mutations. Snapshot is
    /// applied first, then only fresh per-operation sequence events are
    /// reduced; duplicate and stale replay frames are ignored.
    ///
    /// # Errors
    ///
    /// daemon が snapshot/replay barrier を返せない場合に返す。
    pub fn reconcile(&mut self, state: &mut LifecycleState) -> Result<(), ClientError> {
        let request = ReconcileRequest {
            workspace: state.workspace(),
            operations: self
                .intents
                .keys()
                .map(|operation_id| OperationCursor {
                    operation_id: *operation_id,
                    after_sequence: self.sequences.get(operation_id).copied().unwrap_or(0),
                })
                .collect(),
        };
        let snapshot = self.client.reconcile(request)?;
        let _ = update(
            state,
            Event::Snapshot {
                sessions: snapshot.sessions,
            },
        );
        for event in snapshot.events {
            self.apply(state, event);
        }
        Ok(())
    }

    fn apply(&mut self, state: &mut LifecycleState, update_event: SequencedOperationEvent) {
        let last = self
            .sequences
            .get(&update_event.operation_id)
            .copied()
            .unwrap_or(0);
        if update_event.sequence <= last {
            return;
        }
        self.sequences
            .insert(update_event.operation_id, update_event.sequence);
        let event = match update_event.event {
            OperationEvent::Accepted { row } => {
                self.intents
                    .entry(update_event.operation_id)
                    .or_insert_with(|| row.clone());
                DaemonEvent::Accepted {
                    operation_id: update_event.operation_id,
                    row,
                }
            }
            OperationEvent::Progress { revision, message } => DaemonEvent::Progress {
                operation_id: update_event.operation_id,
                revision,
                message,
            },
            OperationEvent::Succeeded { revision, created } => DaemonEvent::Succeeded {
                operation_id: update_event.operation_id,
                revision,
                created,
            },
            OperationEvent::Failed { revision, message } => DaemonEvent::Failed {
                operation_id: update_event.operation_id,
                revision,
                message,
            },
        };
        let _ = update(state, Event::Daemon(event));
    }
}

fn pending_row(state: &LifecycleState, effect: &Effect) -> (OperationId, PendingRow) {
    match effect {
        Effect::Create {
            operation_id,
            label,
            ..
        } => (
            *operation_id,
            PendingRow::Creating {
                label: label.clone(),
            },
        ),
        Effect::Remove {
            operation_id,
            session,
            ..
        } => {
            let row = state
                .sessions()
                .iter()
                .find(|row| row.id == *session)
                .expect("reducer only emits remove effects for known sessions")
                .clone();
            (*operation_id, PendingRow::Removing { row })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::os::unix::net::UnixStream;
    use std::thread;

    use super::super::lifecycle::{Event, Target};
    use super::*;
    use usagi_core::infrastructure::ipc::{
        Bootstrap, BuildIdentity, ConnectionId, DaemonGeneration, Envelope, EnvelopeKind,
        ErrorCode, GenerationRole, ProtocolError, ProtocolLimits, ProtocolVersion, ResponseOutcome,
        ServerHello, read_frame, write_json_frame,
    };
    use usagi_core::usecase::client::{
        ClientPolicy, DaemonClient, DaemonReply, DaemonRequest, IpcClient, SessionAction,
    };

    #[derive(Debug, Default)]
    struct FakeClient {
        submitted: Vec<Effect>,
        accepted: VecDeque<Result<OperationAccepted, ClientError>>,
        reconciles: Vec<ReconcileRequest>,
        snapshots: VecDeque<Result<ReconcileSnapshot, ClientError>>,
    }

    impl SessionLifecycleClient for FakeClient {
        fn submit(&mut self, effect: &Effect) -> Result<OperationAccepted, ClientError> {
            self.submitted.push(effect.clone());
            self.accepted.pop_front().expect("accepted response")
        }

        fn reconcile(
            &mut self,
            request: ReconcileRequest,
        ) -> Result<ReconcileSnapshot, ClientError> {
            self.reconciles.push(request);
            self.snapshots.pop_front().expect("reconcile response")
        }
    }

    fn state(rows: Vec<SessionRow>) -> LifecycleState {
        LifecycleState::new(WorkspaceId::new(), rows)
    }
    fn row(label: &str) -> SessionRow {
        SessionRow {
            id: usagi_core::domain::id::SessionId::new(),
            label: label.into(),
        }
    }
    fn accepted(operation_id: OperationId, revision: u64) -> OperationAccepted {
        OperationAccepted {
            operation_id,
            revision,
        }
    }
    fn snapshot(
        sessions: Vec<SessionRow>,
        events: Vec<SequencedOperationEvent>,
    ) -> ReconcileSnapshot {
        ReconcileSnapshot { sessions, events }
    }

    #[test]
    fn dispatch_preserves_operation_id_and_does_not_resubmit_durable_intent() {
        let operation = OperationId::new();
        let client = FakeClient {
            accepted: VecDeque::from([Ok(accepted(operation, 1))]),
            ..FakeClient::default()
        };
        let mut adapter = SessionLifecycleAdapter::new(client);
        let mut state = state(vec![]);
        let effects = update(
            &mut state,
            Event::RequestCreate {
                operation_id: operation,
                label: "new".into(),
            },
        );
        adapter.dispatch(&mut state, effects.clone()).unwrap();
        adapter.dispatch(&mut state, effects).unwrap();
        assert_eq!(adapter.client().submitted.len(), 1);
        assert_eq!(
            adapter.client().submitted[0],
            Effect::Create {
                workspace: state.workspace(),
                operation_id: operation,
                label: "new".into()
            }
        );
        assert_eq!(state.pending().len(), 1);
        assert_eq!(adapter.client_mut().submitted.len(), 1);
    }

    #[test]
    fn accepted_response_for_another_operation_is_left_for_reconcile() {
        let operation = OperationId::new();
        let client = FakeClient {
            accepted: VecDeque::from([Ok(accepted(OperationId::new(), 1))]),
            ..FakeClient::default()
        };
        let mut adapter = SessionLifecycleAdapter::new(client);
        let mut state = state(vec![]);
        let effects = update(
            &mut state,
            Event::RequestCreate {
                operation_id: operation,
                label: "new".into(),
            },
        );
        adapter.dispatch(&mut state, effects).unwrap();
        assert!(state.pending().is_empty());
    }

    #[test]
    fn reconnect_applies_snapshot_and_only_fresh_replay_events() {
        let operation = OperationId::new();
        let created = row("created");
        let retained = row("retained");
        let client = FakeClient {
            accepted: VecDeque::from([Ok(accepted(operation, 1))]),
            snapshots: VecDeque::from([
                Ok(snapshot(
                    vec![retained.clone()],
                    vec![
                        SequencedOperationEvent {
                            operation_id: operation,
                            sequence: 2,
                            event: OperationEvent::Progress {
                                revision: 1,
                                message: "working".into(),
                            },
                        },
                        SequencedOperationEvent {
                            operation_id: operation,
                            sequence: 3,
                            event: OperationEvent::Succeeded {
                                revision: 2,
                                created: Some(created.clone()),
                            },
                        },
                    ],
                )),
                Ok(snapshot(
                    vec![retained.clone(), created.clone()],
                    vec![SequencedOperationEvent {
                        operation_id: operation,
                        sequence: 3,
                        event: OperationEvent::Succeeded {
                            revision: 9,
                            created: Some(row("stale")),
                        },
                    }],
                )),
            ]),
            ..FakeClient::default()
        };
        let mut adapter = SessionLifecycleAdapter::new(client);
        let mut state = state(vec![]);
        let effects = update(
            &mut state,
            Event::RequestCreate {
                operation_id: operation,
                label: "new".into(),
            },
        );
        adapter.dispatch(&mut state, effects).unwrap();
        adapter.reconcile(&mut state).unwrap();
        adapter.reconcile(&mut state).unwrap();
        assert_eq!(
            adapter.client().reconciles[1].operations,
            vec![OperationCursor {
                operation_id: operation,
                after_sequence: 3
            }]
        );
        assert_eq!(state.sessions(), &[retained, created]);
        assert!(state.pending().is_empty());
    }

    #[test]
    fn reconnect_restores_unknown_accepted_operation_and_snapshot_falls_back_to_root() {
        let operation = OperationId::new();
        let missing = row("missing");
        let client = FakeClient {
            snapshots: VecDeque::from([Ok(snapshot(
                vec![],
                vec![SequencedOperationEvent {
                    operation_id: operation,
                    sequence: 1,
                    event: OperationEvent::Accepted {
                        row: PendingRow::Creating {
                            label: "restored".into(),
                        },
                    },
                }],
            ))]),
            ..FakeClient::default()
        };
        let mut adapter = SessionLifecycleAdapter::new(client);
        let mut state = state(vec![missing.clone()]);
        let selected_operation = OperationId::new();
        let _ = update(
            &mut state,
            Event::Daemon(DaemonEvent::Accepted {
                operation_id: selected_operation,
                row: PendingRow::Creating {
                    label: missing.label.clone(),
                },
            }),
        );
        let _ = update(
            &mut state,
            Event::Daemon(DaemonEvent::Succeeded {
                operation_id: selected_operation,
                revision: 1,
                created: Some(missing),
            }),
        );
        let _ = update(&mut state, Event::Snapshot { sessions: vec![] });
        assert_eq!(state.active(), Target::Root);
        adapter.reconcile(&mut state).unwrap();
        assert_eq!(state.pending().len(), 1);
    }

    #[test]
    fn unavailable_submit_is_not_retried_and_reconcile_error_is_returned() {
        let operation = OperationId::new();
        let unavailable = ClientError::Unavailable("closed".into());
        let client = FakeClient {
            accepted: VecDeque::from([Err(unavailable.clone())]),
            snapshots: VecDeque::from([Err(ClientError::Protocol(ProtocolError::new(
                ErrorCode::Unavailable,
                "later",
            )))]),
            ..FakeClient::default()
        };
        let mut adapter = SessionLifecycleAdapter::new(client);
        let mut state = state(vec![]);
        let effects = update(
            &mut state,
            Event::RequestCreate {
                operation_id: operation,
                label: "new".into(),
            },
        );
        assert_eq!(
            adapter.dispatch(&mut state, effects.clone()),
            Err(unavailable)
        );
        adapter.dispatch(&mut state, effects).unwrap();
        assert_eq!(adapter.client().submitted.len(), 1);
        assert!(matches!(
            adapter.reconcile(&mut state),
            Err(ClientError::Protocol(_))
        ));
    }

    #[test]
    fn remove_uses_original_row_and_terminal_replay_is_ignored() {
        let original = row("original");
        let operation = OperationId::new();
        let client = FakeClient {
            accepted: VecDeque::from([Ok(accepted(operation, 1))]),
            snapshots: VecDeque::from([Ok(snapshot(
                vec![original.clone()],
                vec![
                    SequencedOperationEvent {
                        operation_id: operation,
                        sequence: 2,
                        event: OperationEvent::Failed {
                            revision: 1,
                            message: "no".into(),
                        },
                    },
                    SequencedOperationEvent {
                        operation_id: operation,
                        sequence: 3,
                        event: OperationEvent::Progress {
                            revision: 2,
                            message: "late".into(),
                        },
                    },
                ],
            ))]),
            ..FakeClient::default()
        };
        let mut adapter = SessionLifecycleAdapter::new(client);
        let mut state = state(vec![original.clone()]);
        let effects = update(
            &mut state,
            Event::RequestRemove {
                operation_id: operation,
                session: original.id,
            },
        );
        adapter.dispatch(&mut state, effects).unwrap();
        adapter.reconcile(&mut state).unwrap();
        assert_eq!(state.sessions(), &[original]);
        assert_eq!(state.error(), Some("no"));
        assert!(state.pending().is_empty());
    }

    #[test]
    fn unix_socket_acceptance_keeps_the_operation_identity() {
        let operation = OperationId::new();
        let expected = operation.to_string();
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        let server = thread::spawn(move || {
            let _ = read_frame(&mut server_stream).unwrap();
            let protocol = ProtocolVersion {
                generation: 1,
                revision: 1,
            };
            let generation = DaemonGeneration("test-daemon".into());
            write_json_frame(
                &mut server_stream,
                &Bootstrap::ServerHello(ServerHello {
                    connection_nonce: "nonce".into(),
                    connection_id: ConnectionId("socket".into()),
                    daemon_generation: generation.clone(),
                    generation_role: GenerationRole::Active,
                    protocol,
                    capabilities: vec![],
                    build: BuildIdentity {
                        version: "test".into(),
                        commit: "test".into(),
                        target: "test".into(),
                        artifact: "server-artifact".into(),
                    },
                    limits: ProtocolLimits::default(),
                }),
                1_048_576,
            )
            .unwrap();
            let _ = read_frame(&mut server_stream).unwrap();
            write_json_frame(
                &mut server_stream,
                &Envelope {
                    protocol,
                    daemon_generation: generation,
                    kind: EnvelopeKind::Response {
                        request_id: usagi_core::infrastructure::ipc::RequestId("1".into()),
                        outcome: ResponseOutcome::Accepted {
                            operation_id: usagi_core::infrastructure::ipc::OperationId(expected),
                            operation_revision: 1,
                        },
                        body: serde_json::json!(null),
                    },
                },
                1_048_576,
            )
            .unwrap();
        });
        let mut client = IpcClient::connect(
            client_stream,
            "tui".into(),
            "nonce".into(),
            ClientPolicy::tui(),
            BuildIdentity {
                version: "test".into(),
                commit: "test".into(),
                target: "test".into(),
                artifact: "client-artifact".into(),
            },
        )
        .unwrap();
        let reply = client
            .request(DaemonRequest::Session {
                action: SessionAction::Create,
                operation_id: operation.to_string(),
                payload: serde_json::json!({"label": "new"}),
            })
            .unwrap();
        server.join().unwrap();
        assert_eq!(
            reply,
            DaemonReply::Accepted {
                operation_id: operation.to_string(),
                revision: 1,
                body: serde_json::json!(null),
            }
        );
    }
}

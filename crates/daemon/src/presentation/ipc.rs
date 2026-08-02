//! Handshake-gated server adapter for the transport-independent IPC protocol.

#![allow(clippy::missing_errors_doc)] // Errors are directly forwarded transport/protocol failures.

use std::io::{self, Read, Write};

use serde_json::json;
use usagi_core::infrastructure::ipc::{
    Bootstrap, DaemonGeneration, Envelope, EnvelopeKind, ErrorCode, OperationId, ProtocolError,
    ResponseOutcome, ServerHello, ServerProtocol, negotiate, read_json_frame, write_json_frame,
};

/// Daemon-owned terminal actor port.  The transport never interprets a
/// terminal payload itself and therefore cannot accidentally echo it or turn a
/// failed request into a local fallback.  Implementations are serialized by
/// the composition root and may own a generic coordinator, profile resolver,
/// durable store and PTY adapter.
pub trait TerminalOwner {
    /// Handles one terminal request. `wire` is the snapshot payload this
    /// connection negotiated, so an attach / resync / resize response carries
    /// either the legacy raw tail or the semantic screen checkpoint — never a
    /// payload the peer did not negotiate.
    fn request(
        &mut self,
        connection: usagi_core::domain::id::ConnectionId,
        client: usagi_core::domain::id::ClientId,
        request_id: usagi_core::domain::id::RequestId,
        action: usagi_core::usecase::client::TerminalAction,
        payload: serde_json::Value,
        wire: crate::usecase::terminal::SnapshotWire,
    ) -> Result<serde_json::Value, ProtocolError>;
    /// Lists the daemon-owned runtimes this owner holds in the exact requested
    /// scope. The default owner holds none; the generic terminal owner returns
    /// its running terminals. `SharedTerminalOwner` merges this with the Agent
    /// owner so a client's `Inventory` request sees both kinds.
    fn inventory(
        &self,
        _scope: &usagi_core::domain::terminal_launch::TerminalLaunchScope,
    ) -> Vec<usagi_core::domain::terminal_launch::TerminalInventoryEntry> {
        Vec::new()
    }
    /// Lists this owner's exited tombstones in the exact requested scope (#525).
    /// The default owner holds none; the generic terminal owner returns its
    /// exited terminals. `SharedTerminalOwner` merges this with the Agent owner
    /// and stamps each entry's authoritative workspace-global visibility.
    fn completed_inventory(
        &self,
        _scope: &usagi_core::domain::terminal_launch::TerminalLaunchScope,
    ) -> Vec<usagi_core::domain::terminal_visibility::CompletedTerminalEntry> {
        Vec::new()
    }
    fn disconnect(&mut self, connection: usagi_core::domain::id::ConnectionId);
}

/// The generation authority one client connection is served under.
///
/// It is one port rather than three parameters because its three verbs are one
/// responsibility observed at three moments of a connection's life, and getting
/// any of them wrong breaks the same invariant:
///
/// | verb | moment | what it protects |
/// |---|---|---|
/// | [`admitted`](Self::admitted) | after the handshake | a rollover may only leave a draining generation behind if *every* live connection can address it ([`routing`]) |
/// | [`admit`](Self::admit) | before each request is dispatched | authority is re-decided per request, so a connection opened under a previous role gains nothing from having got in ([`admission`]) |
/// | [`disconnected`](Self::disconnected) | when the connection ends | a client that has gone away must stop blocking a rollover |
///
/// [`admission`]: crate::usecase::authority::admission
/// [`routing`]: crate::usecase::authority::routing
pub trait ConnectionFence {
    /// Record what an admitted client advertised.
    fn admitted(
        &self,
        connection: usagi_core::domain::id::ConnectionId,
        hello: &usagi_core::infrastructure::ipc::ClientHello,
    );

    /// Admit one request body and return the lease to hold across its dispatch.
    ///
    /// `Ok(None)` is an admitted request that needs no lease: it produces no
    /// effect a handoff barrier could have to wait for. The connection loop holds
    /// whatever it gets until the reply is written and must not re-check it —
    /// re-checking authority after an effect cannot un-spawn a process, which is
    /// the whole reason this runs first.
    ///
    /// # Errors
    /// Returns the typed refusal that fails the request closed. Every refusal is
    /// effect zero.
    fn admit(
        &self,
        body: &serde_json::Value,
    ) -> Result<Option<crate::usecase::authority::admission::AdmissionLease>, ProtocolError>;

    /// Forget a connection that has gone away.
    fn disconnected(&self, connection: usagi_core::domain::id::ConnectionId);
}

/// A fence that admits everything and remembers nothing.
///
/// It is for callers that hold no generation authority to speak for — the
/// protocol-level tests in this module, which exercise the transport rather than
/// the authority. Production `serve` always passes a real fence: the parameter is
/// required precisely so a new call site cannot end up unfenced by omission.
pub struct UnfencedConnection;

impl ConnectionFence for UnfencedConnection {
    fn admitted(
        &self,
        _connection: usagi_core::domain::id::ConnectionId,
        _hello: &usagi_core::infrastructure::ipc::ClientHello,
    ) {
    }

    fn admit(
        &self,
        _body: &serde_json::Value,
    ) -> Result<Option<crate::usecase::authority::admission::AdmissionLease>, ProtocolError> {
        Ok(None)
    }

    fn disconnected(&self, _connection: usagi_core::domain::id::ConnectionId) {}
}

/// An admitted connection and the peer identity durable per-client daemon state
/// is bound to.
///
/// The connection is not that identity. A client keeps working across reconnects,
/// so anything it must still reach after a lost response — notably the terminal
/// input operation ledger (#519) — has to be keyed by the client incarnation the
/// hello declares, not by the socket that happened to carry the request.
#[derive(Debug, Clone, PartialEq)]
pub struct AdmittedConnection {
    pub hello: ServerHello,
    /// The peer's declared client incarnation, when it is a canonical resource
    /// identity. `None` is a peer that predates the ledger: its terminal input
    /// stays on the connection-local sequence contract and it may not present a
    /// durable operation identity.
    pub client_incarnation: Option<usagi_core::domain::id::ClientId>,
    /// What the peer itself declared, retained because the *negotiated*
    /// [`ServerHello`] cannot answer for it: a rollover asks whether every live
    /// client can address a draining generation, and only the client's own
    /// capability list says so ([`ConnectionFence::admitted`]).
    pub client: usagi_core::infrastructure::ipc::ClientHello,
}

/// Complete a bootstrap handshake. No ordinary envelope is accepted before this succeeds.
pub fn handshake(
    reader: &mut dyn Read,
    writer: &mut dyn Write,
    server: &ServerProtocol,
) -> io::Result<Option<ServerHello>> {
    Ok(handshake_admitted(reader, writer, server)?.map(|admitted| admitted.hello))
}

/// As [`handshake`], but also reports the client incarnation durable per-client
/// state is keyed by, and the hello the peer declared.
pub fn handshake_admitted(
    reader: &mut dyn Read,
    writer: &mut dyn Write,
    server: &ServerProtocol,
) -> io::Result<Option<AdmittedConnection>> {
    let Some(first) = read_json_frame::<Bootstrap>(reader, server.limits.max_frame_bytes as usize)?
    else {
        return Ok(None);
    };
    let Bootstrap::ClientHello(hello) = first else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "client hello must be the first frame",
        ));
    };
    match negotiate(&hello, server) {
        Ok(reply) => {
            write_json_frame(
                writer,
                &Bootstrap::ServerHello(reply.clone()),
                server.limits.max_frame_bytes as usize,
            )?;
            Ok(Some(AdmittedConnection {
                hello: reply,
                client_incarnation: usagi_core::domain::id::ClientId::parse(&hello.client_id.0)
                    .ok(),
                client: hello,
            }))
        }
        Err(error) => {
            write_json_frame(
                writer,
                &Bootstrap::Error(error),
                server.limits.max_frame_bytes as usize,
            )?;
            Ok(None)
        }
    }
}

/// Dispatch requests without leaking presentation-local state mutation back to
/// callers. Session and Agent operations are admitted durably by their
/// producer-supplied operation id; terminal requests retain their typed body
/// for the terminal owner to process.
#[must_use]
pub fn dispatch(
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: serde_json::Value,
    hello: &ServerHello,
) -> Envelope {
    let kind = body.get("kind").and_then(serde_json::Value::as_str);
    let (outcome, body) = if matches!(kind, Some("dispatch_tool" | "supervisor_tool")) {
        (
            ResponseOutcome::Error(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "daemon tool action is not implemented",
            )),
            json!(null),
        )
    } else {
        let outcome = kind
            .filter(|kind| {
                matches!(
                    *kind,
                    "rollover" | "session" | "agent" | "resume_agent" | "dispatch"
                )
            })
            .and_then(|_| body.get("operation_id"))
            .and_then(serde_json::Value::as_str)
            .map_or(ResponseOutcome::Ok, |operation_id| {
                ResponseOutcome::Accepted {
                    operation_id: OperationId(operation_id.to_owned()),
                    operation_revision: 1,
                }
            });
        (outcome, body)
    };
    Envelope {
        protocol: hello.protocol,
        daemon_generation: hello.daemon_generation.clone(),
        kind: EnvelopeKind::Response {
            request_id,
            outcome,
            body,
        },
    }
}

/// Serve one client. A target generation mismatch and pre-handshake normal
/// request are rejected before request dispatch.
pub fn handle_connection(
    reader: &mut dyn Read,
    writer: &mut dyn Write,
    server: &ServerProtocol,
) -> io::Result<()> {
    let mut dispatch_request = dispatch;
    handle_connection_with(reader, writer, server, &mut dispatch_request)
}

/// As [`handle_connection`], but routes accepted requests to the daemon-owned
/// runtime supplied by the composition root.  Keeping the runtime outside the
/// connection makes durable state shared by every client connection.
pub fn handle_connection_with(
    reader: &mut dyn Read,
    writer: &mut dyn Write,
    server: &ServerProtocol,
    dispatch_request: &mut dyn FnMut(
        usagi_core::infrastructure::ipc::RequestId,
        serde_json::Value,
        &ServerHello,
    ) -> Envelope,
) -> io::Result<()> {
    let Some(hello) = handshake(reader, writer, server)? else {
        return Ok(());
    };
    while let Some(envelope) =
        read_json_frame::<Envelope>(reader, hello.limits.max_frame_bytes as usize)?
    {
        let EnvelopeKind::Request {
            request_id, body, ..
        } = envelope.kind
        else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "client may only send request envelopes",
            ));
        };
        if envelope.protocol != hello.protocol
            || envelope.daemon_generation != hello.daemon_generation
        {
            let error = ProtocolError::new(
                ErrorCode::GenerationMismatch,
                "request targets a different daemon generation",
            );
            let reply = Envelope {
                protocol: hello.protocol,
                daemon_generation: hello.daemon_generation.clone(),
                kind: EnvelopeKind::Response {
                    request_id,
                    outcome: usagi_core::infrastructure::ipc::ResponseOutcome::Error(error),
                    body: json!(null),
                },
            };
            write_json_frame(writer, &reply, hello.limits.max_frame_bytes as usize)?;
            continue;
        }
        let reply = dispatch_request(request_id, body, &hello);
        write_json_frame(writer, &reply, hello.limits.max_frame_bytes as usize)?;
    }
    Ok(())
}

/// Serve one client with a shared terminal owner while preserving the caller's
/// non-terminal dispatch.  The composition root uses this to keep session
/// lifecycle routing independent from daemon-owned PTY ownership.
///
/// `fence` is this generation's authority over the connection. It runs *before*
/// every dispatch — including the terminal path, which never reaches
/// `dispatch_request` — and the permit it issues is held until the reply has been
/// written. That ordering is the contract: a request refused here has produced no
/// effect, and a request admitted here cannot have its lease revoked underneath
/// it while its effect is still in flight.
pub fn handle_connection_with_terminal_and(
    reader: &mut dyn Read,
    writer: &mut dyn Write,
    server: &ServerProtocol,
    fence: &dyn ConnectionFence,
    terminal: &mut dyn TerminalOwner,
    dispatch_request: &mut dyn FnMut(
        usagi_core::infrastructure::ipc::RequestId,
        serde_json::Value,
        &ServerHello,
        usagi_core::domain::id::ConnectionId,
        usagi_core::domain::id::ClientId,
    ) -> Envelope,
) -> io::Result<()> {
    let Some(admitted) = handshake_admitted(reader, writer, server)? else {
        return Ok(());
    };
    handle_admitted_connection_with_terminal_and(
        reader,
        writer,
        admitted,
        fence,
        terminal,
        dispatch_request,
    )
}

/// Serve a connection whose complete hello has already been read and answered.
///
/// The production Unix adapter uses this boundary to hold a daemon-wide permit
/// and one absolute socket deadline for exactly the pre-handshake phase, then
/// remove both before entering the established-connection policy below. Keeping
/// the admitted hello as the input also ensures generation, workspace,
/// capability, and credential decisions still come exclusively from
/// [`handshake_admitted`].
pub fn handle_admitted_connection_with_terminal_and(
    reader: &mut dyn Read,
    writer: &mut dyn Write,
    admitted: AdmittedConnection,
    fence: &dyn ConnectionFence,
    terminal: &mut dyn TerminalOwner,
    dispatch_request: &mut dyn FnMut(
        usagi_core::infrastructure::ipc::RequestId,
        serde_json::Value,
        &ServerHello,
        usagi_core::domain::id::ConnectionId,
        usagi_core::domain::id::ClientId,
    ) -> Envelope,
) -> io::Result<()> {
    let AdmittedConnection {
        hello,
        client_incarnation,
        client: client_hello,
    } = admitted;
    let connection = usagi_core::domain::id::ConnectionId::new();
    // The ledger key is the client incarnation the peer declared, so a client
    // that reconnects still reaches the input operations it already issued. A
    // peer without one gets a connection-local identity, which keeps its
    // sequence ledger working and leaves it unable to replay anything.
    let client = client_incarnation.unwrap_or_else(usagi_core::domain::id::ClientId::new);
    fence.admitted(connection, &client_hello);
    let result = (|| {
        while let Some(envelope) =
            read_json_frame::<Envelope>(reader, hello.limits.max_frame_bytes as usize)?
        {
            let EnvelopeKind::Request {
                request_id, body, ..
            } = envelope.kind
            else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "client may only send request envelopes",
                ));
            };
            let outcome_body = if envelope.protocol != hello.protocol
                || envelope.daemon_generation != hello.daemon_generation
            {
                // A request that does not even target this generation is answered
                // without consulting the fence: taking a lease for work that will
                // not happen would make a handoff barrier wait on nothing.
                Err(ProtocolError::new(
                    ErrorCode::GenerationMismatch,
                    "request targets a different daemon generation",
                ))
            } else {
                match fence.admit(&body) {
                    // `_lease` is bound for the whole arm, so it is released only
                    // after the effect below has finished — never between the
                    // authority check and the effect it authorized.
                    Ok(_lease) => {
                        if let Ok(usagi_core::usecase::client::DaemonRequest::Terminal {
                            action,
                            payload,
                        }) = serde_json::from_value(body.clone())
                        {
                            if client_incarnation.is_none() && carries_input_operation(&payload) {
                                Err(ProtocolError::new(
                                    ErrorCode::Unauthenticated,
                                    "durable terminal input requires a canonical client incarnation",
                                ))
                            } else {
                                match usagi_core::domain::id::RequestId::parse(&request_id.0) {
                                    Ok(owner_request_id) => terminal
                                        .request(
                                            connection,
                                            client,
                                            owner_request_id,
                                            action,
                                            payload,
                                            crate::usecase::terminal::SnapshotWire::for_revision(
                                                hello.protocol.revision,
                                            ),
                                        )
                                        .map(ok_response),
                                    Err(_) => Err(ProtocolError::new(
                                        ErrorCode::InvalidArgument,
                                        "terminal request_id must be a canonical resource ID",
                                    )),
                                }
                            }
                        } else {
                            let dispatched = dispatch_request(
                                request_id.clone(),
                                body.clone(),
                                &hello,
                                connection,
                                client,
                            );
                            // Session, agent, and metrics dispatchers each own their
                            // outcome.  Replacing a session error with `Ok(null)` makes a
                            // client mistake the error body for a lifecycle snapshot.
                            Ok(dispatched.kind_response())
                        }
                    }
                    Err(refusal) => Err(refusal),
                }
            };
            let (outcome, body) = match outcome_body {
                Ok((outcome, body)) => (outcome, body),
                Err(error) => (ResponseOutcome::Error(error), json!(null)),
            };
            let reply = Envelope {
                protocol: hello.protocol,
                daemon_generation: hello.daemon_generation.clone(),
                kind: EnvelopeKind::Response {
                    request_id,
                    outcome,
                    body,
                },
            };
            write_json_frame(writer, &reply, hello.limits.max_frame_bytes as usize)?;
        }
        Ok(())
    })();
    terminal.disconnect(connection);
    // A connection that has gone away must stop blocking a rollover, so the fence
    // forgets it on every exit from the loop — a clean end, a protocol error, and
    // a transport failure alike.
    fence.disconnected(connection);
    result
}

/// Whether a terminal payload asks for cross-connection operation semantics.
///
/// A peer whose client incarnation could not be established cannot be given
/// those semantics: its ledger would be keyed by a connection-local identity, so
/// a later "replay" would look like a new operation and reach the PTY twice. The
/// request is refused before the owner sees it rather than degraded silently.
fn carries_input_operation(payload: &serde_json::Value) -> bool {
    payload
        .get("input_operation")
        .is_some_and(|operation| !operation.is_null())
}

fn ok_response(body: serde_json::Value) -> (ResponseOutcome, serde_json::Value) {
    (ResponseOutcome::Ok, body)
}

trait ResponseOutcomeBody {
    fn kind_response(self) -> (ResponseOutcome, serde_json::Value);
}
impl ResponseOutcomeBody for Envelope {
    fn kind_response(self) -> (ResponseOutcome, serde_json::Value) {
        match self.kind {
            EnvelopeKind::Response { outcome, body, .. } => (outcome, body),
            _ => (ResponseOutcome::Ok, json!(null)),
        }
    }
}

/// Build a server protocol policy from daemon-owned identity/configuration.
///
/// `workspace_root` is the canonical root this daemon took authority over at
/// startup. It is the only workspace it can serve, so the handshake refuses a
/// client that declares a different one; a root that cannot be spelled on the
/// wire is passed as empty and refuses every workspace-bound client.
#[must_use]
pub fn server_protocol(
    daemon_generation: DaemonGeneration,
    connection_id: String,
    build: usagi_core::infrastructure::ipc::BuildIdentity,
    daemon_process: usagi_core::domain::daemon::DaemonRecord,
    workspace_root: String,
) -> ServerProtocol {
    ServerProtocol {
        daemon_generation,
        connection_id: usagi_core::infrastructure::ipc::ConnectionId(connection_id),
        generation_role: usagi_core::infrastructure::ipc::GenerationRole::Active,
        supported_protocols: vec![usagi_core::infrastructure::ipc::ProtocolRange {
            generation: usagi_core::infrastructure::ipc::TERMINAL_WIRE_GENERATION,
            min_revision: 0,
            // Revision 2 adds the semantic screen checkpoint; a client that
            // negotiates a lower revision keeps the legacy raw tail.
            max_revision: usagi_core::infrastructure::ipc::TERMINAL_CHECKPOINT_REVISION,
        }],
        capabilities: usagi_core::infrastructure::ipc::server_advertised_capabilities(
            usagi_core::infrastructure::ipc::GenerationRole::Active,
        ),
        build,
        limits: usagi_core::infrastructure::ipc::ProtocolLimits::default(),
        daemon_process: Some(daemon_process),
        workspace_root,
    }
}

/// Build the server protocol a **standby** generation answers its readiness
/// handshake with.
///
/// Two things differ from the active policy, and both are what makes readiness
/// meaningful rather than decorative:
///
/// * the role is [`GenerationRole::Standby`], so a client that somehow reached
///   this private endpoint cannot bind it as the data directory's owner (owner
///   binding requires `active`);
/// * it advertises
///   [`Capability::GenerationHandoff`](usagi_core::infrastructure::ipc::Capability::GenerationHandoff),
///   which is the peer's claim that it participates in the durable registry and
///   re-decides authority per request. A standby that could not honour role
///   admission must not be namable as a successor, and
///   [`verify_readiness`](crate::usecase::authority::standby::verify_readiness)
///   refuses a hello without it.
///
/// The active policy deliberately does *not* advertise that capability: this
/// build's active generation registers itself
/// ([`crate::usecase::authority::activation`]) but serves its requests without a
/// role-admission barrier, so claiming otherwise would be a claim a rollover
/// would then trust.
#[must_use]
pub fn standby_server_protocol(
    daemon_generation: DaemonGeneration,
    connection_id: String,
    build: usagi_core::infrastructure::ipc::BuildIdentity,
    daemon_process: usagi_core::domain::daemon::DaemonRecord,
    workspace_root: String,
) -> ServerProtocol {
    let mut protocol = server_protocol(
        daemon_generation,
        connection_id,
        build,
        daemon_process,
        workspace_root,
    );
    protocol.generation_role = usagi_core::infrastructure::ipc::GenerationRole::Standby;
    protocol.capabilities = usagi_core::infrastructure::ipc::server_advertised_capabilities(
        usagi_core::infrastructure::ipc::GenerationRole::Standby,
    );
    protocol
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use usagi_core::infrastructure::ipc::{
        BuildIdentity, ClientHello, ClientId, ClientWorkspace, ProtocolRange, ProtocolVersion,
        WORKSPACE_FENCE_CAPABILITY, is_workspace_mismatch, read_json_frame, write_json_frame,
    };

    struct BrokenWriter;
    impl Write for BrokenWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("broken"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingTerminal {
        fail: bool,
        requests: usize,
        disconnects: usize,
        wires: Vec<crate::usecase::terminal::SnapshotWire>,
        /// The client incarnation each routed request was attributed to.
        clients: Vec<usagi_core::domain::id::ClientId>,
    }
    impl TerminalOwner for RecordingTerminal {
        fn request(
            &mut self,
            _: usagi_core::domain::id::ConnectionId,
            client: usagi_core::domain::id::ClientId,
            _: usagi_core::domain::id::RequestId,
            _: usagi_core::usecase::client::TerminalAction,
            _: serde_json::Value,
            wire: crate::usecase::terminal::SnapshotWire,
        ) -> Result<serde_json::Value, ProtocolError> {
            self.requests += 1;
            self.wires.push(wire);
            self.clients.push(client);
            if self.fail {
                Err(ProtocolError::new(
                    ErrorCode::Unavailable,
                    "terminal failed",
                ))
            } else {
                Ok(json!({"terminal": "handled"}))
            }
        }

        fn disconnect(&mut self, _: usagi_core::domain::id::ConnectionId) {
            self.disconnects += 1;
        }
    }

    /// The workspace root the fixture daemon owns.
    const TRUSTED_ROOT: &str = "/workspace/root";

    fn server() -> ServerProtocol {
        server_protocol(
            DaemonGeneration("current".into()),
            "conn".into(),
            BuildIdentity {
                version: "1".into(),
                commit: "x".into(),
                target: "test".into(),
                artifact: "server-artifact".into(),
            },
            usagi_core::domain::daemon::DaemonRecord::identified(2, "test-process"),
            TRUSTED_ROOT.to_owned(),
        )
    }
    /// A standby answers a readiness handshake with the same policy in every
    /// respect but two: the role it names itself, and the capability that says it
    /// re-decides authority per request.
    #[test]
    fn a_standby_names_its_role_and_advertises_the_handoff_capability() {
        let active = server();
        let standby = standby_server_protocol(
            active.daemon_generation.clone(),
            active.connection_id.0.clone(),
            active.build.clone(),
            active
                .daemon_process
                .clone()
                .expect("the fixture asserts an owner process"),
            active.workspace_root.clone(),
        );
        assert_eq!(
            standby.generation_role,
            usagi_core::infrastructure::ipc::GenerationRole::Standby
        );
        // Owner binding requires `active`, so a client can never mistake a
        // standby's private endpoint for the data directory's authority.
        assert_eq!(
            active.generation_role,
            usagi_core::infrastructure::ipc::GenerationRole::Active
        );
        let required = usagi_core::infrastructure::ipc::Capability::GenerationHandoff.wire_name();
        assert!(standby.capabilities.iter().any(|it| it == required));
        assert!(!active.capabilities.iter().any(|it| it == required));
        // Readiness compares the artifact byte for byte, so the standby must
        // advertise exactly the artifact it was admitted for.
        assert_eq!(standby.build, active.build);
        assert_eq!(standby.workspace_root, active.workspace_root);
    }

    fn hello() -> Bootstrap {
        Bootstrap::ClientHello(client_hello())
    }
    fn client_hello() -> ClientHello {
        ClientHello {
            client_id: ClientId("client".into()),
            connection_nonce: "n".into(),
            expected_daemon_generation: None,
            supported_protocols: vec![ProtocolRange {
                generation: 1,
                min_revision: 0,
                max_revision: 1,
            }],
            capabilities: vec![],
            required_capabilities: vec![
                usagi_core::infrastructure::ipc::Capability::RequestCorrelation
                    .wire_name()
                    .into(),
            ],
            build: BuildIdentity {
                version: "other".into(),
                commit: "y".into(),
                target: "test".into(),
                artifact: "client-artifact".into(),
            },
            workspace: Some(ClientWorkspace::Bound {
                root: TRUSTED_ROOT.to_owned(),
            }),
        }
    }
    fn request() -> Envelope {
        Envelope {
            protocol: ProtocolVersion {
                generation: 1,
                revision: 1,
            },
            daemon_generation: DaemonGeneration("current".into()),
            kind: EnvelopeKind::Request {
                request_id: usagi_core::infrastructure::ipc::RequestId("r".into()),
                timeout_ms: None,
                body: json!({"request":"value"}),
            },
        }
    }
    fn terminal_request(request_id: String) -> Envelope {
        Envelope {
            protocol: ProtocolVersion {
                generation: 1,
                revision: 1,
            },
            daemon_generation: DaemonGeneration("current".into()),
            kind: EnvelopeKind::Request {
                request_id: usagi_core::infrastructure::ipc::RequestId(request_id),
                timeout_ms: None,
                body: serde_json::to_value(usagi_core::usecase::client::DaemonRequest::Terminal {
                    action: usagi_core::usecase::client::TerminalAction::Inventory,
                    payload: json!({}),
                })
                .unwrap(),
            },
        }
    }
    fn test_dispatch(
        request_id: usagi_core::infrastructure::ipc::RequestId,
        body: serde_json::Value,
        hello: &ServerHello,
        _: usagi_core::domain::id::ConnectionId,
        _: usagi_core::domain::id::ClientId,
    ) -> Envelope {
        dispatch(request_id, body, hello)
    }
    #[test]
    fn handshake_returns_hello_and_preserves_build_as_diagnostic() {
        let mut input = Vec::new();
        write_json_frame(&mut input, &hello(), 1024).unwrap();
        let mut output = Vec::new();
        let result = handshake(&mut Cursor::new(input), &mut output, &server())
            .unwrap()
            .unwrap();
        assert_eq!(result.protocol.revision, 1);
        assert!(matches!(
            read_json_frame::<Bootstrap>(&mut Cursor::new(output), 1024).unwrap(),
            Some(Bootstrap::ServerHello(_))
        ));
    }
    /// A fence that refuses everything, and counts what it was asked.
    ///
    /// `UnfencedConnection` can only prove the admitted path; this is the other
    /// half of the seam's contract, and the refusal is where the whole thing earns
    /// its place — a request the fence rejects must reach nothing.
    #[derive(Default)]
    struct RefusingConnection {
        admitted: std::cell::Cell<usize>,
        disconnected: std::cell::Cell<usize>,
    }

    impl ConnectionFence for RefusingConnection {
        fn admitted(
            &self,
            _connection: usagi_core::domain::id::ConnectionId,
            _hello: &ClientHello,
        ) {
            self.admitted.set(self.admitted.get() + 1);
        }

        fn admit(
            &self,
            _body: &serde_json::Value,
        ) -> Result<Option<crate::usecase::authority::admission::AdmissionLease>, ProtocolError>
        {
            Err(ProtocolError::new(
                ErrorCode::GenerationRolledOver,
                "generation stopped admitting this work",
            ))
        }

        fn disconnected(&self, _connection: usagi_core::domain::id::ConnectionId) {
            self.disconnected.set(self.disconnected.get() + 1);
        }
    }

    /// A refused request is answered with the fence's own typed error and reaches
    /// **neither** the terminal owner nor the dispatcher.
    ///
    /// Both are asserted because the fence sits ahead of two different paths: a
    /// terminal request never reaches `dispatch_request` at all, so a fence placed
    /// in the dispatch closure would leave terminal IO unfenced. Each round below
    /// sends one request of each kind.
    ///
    /// The unfenced round runs first and is not a formality: it establishes that
    /// this fixture's dispatcher and terminal owner *are* reachable, without which
    /// the refused round's zero counts would be satisfied by a broken fixture.
    #[test]
    fn a_refused_request_reaches_neither_the_terminal_owner_nor_the_dispatcher() {
        let round = |fence: &dyn ConnectionFence| {
            let mut input = Vec::new();
            write_json_frame(&mut input, &hello(), 1024).unwrap();
            write_json_frame(&mut input, &request(), 1024).unwrap();
            write_json_frame(
                &mut input,
                &terminal_request(usagi_core::domain::id::RequestId::new().as_str()),
                1024,
            )
            .unwrap();

            let mut terminal = RecordingTerminal::default();
            let mut dispatched = 0_usize;
            let mut output = Vec::new();
            handle_connection_with_terminal_and(
                &mut Cursor::new(input),
                &mut output,
                &server(),
                fence,
                &mut terminal,
                &mut |request_id, body, hello, _connection, _client| {
                    dispatched += 1;
                    dispatch(request_id, body, hello)
                },
            )
            .unwrap();

            let mut replies = Cursor::new(output);
            let _ = read_json_frame::<Bootstrap>(&mut replies, 1024).unwrap();
            let mut answered = Vec::new();
            while let Some(envelope) = read_json_frame::<Envelope>(&mut replies, 1024).unwrap() {
                answered.push(envelope.kind);
            }
            (answered, dispatched, terminal)
        };

        // Reachable: the non-terminal request lands in the dispatcher and the
        // terminal one lands on the owner.
        let (served, dispatched, terminal) = round(&UnfencedConnection);
        assert_eq!(served.len(), 2);
        assert_eq!(dispatched, 1);
        assert_eq!(terminal.requests, 1);

        // Refused: the same two requests reach neither, and both are still
        // answered — with the fence's typed error and a null body, so no client
        // can mistake a refusal for a served request.
        let fence = RefusingConnection::default();
        let (refused, dispatched, terminal) = round(&fence);
        assert_eq!(refused.len(), 2);
        for kind in &refused {
            assert!(
                matches!(
                    kind,
                    EnvelopeKind::Response {
                        outcome: ResponseOutcome::Error(error),
                        body,
                        ..
                    } if error.code == ErrorCode::GenerationRolledOver && body.is_null()
                ),
                "{kind:?}"
            );
        }
        assert_eq!(dispatched, 0);
        assert_eq!(terminal.requests, 0);
        // The connection is still admitted to the ledger and forgotten at the end,
        // whatever its requests were answered with.
        assert_eq!(fence.admitted.get(), 1);
        assert_eq!(fence.disconnected.get(), 1);
        assert_eq!(terminal.disconnects, 1);
    }

    #[test]
    fn connection_requires_hello_then_correlates_response() {
        let mut input = Vec::new();
        write_json_frame(&mut input, &hello(), 1024).unwrap();
        write_json_frame(&mut input, &request(), 1024).unwrap();
        let mut output = Vec::new();
        handle_connection(&mut Cursor::new(input), &mut output, &server()).unwrap();
        let mut output = Cursor::new(output);
        let _ = read_json_frame::<Bootstrap>(&mut output, 1024).unwrap();
        let response = read_json_frame::<Envelope>(&mut output, 1024)
            .unwrap()
            .unwrap();
        assert!(matches!(response.kind, EnvelopeKind::Response { .. }));
    }

    /// Durable per-client state must key on the peer's declared incarnation, not
    /// on the socket: a client that reconnects has to reach the terminal input
    /// operations it already issued (#519).
    #[test]
    fn terminal_requests_are_attributed_to_the_declared_client_incarnation() {
        let incarnation = usagi_core::domain::id::ClientId::new();
        let mut durable = client_hello();
        durable.client_id = ClientId(incarnation.as_str());
        let terminal_request = |body: serde_json::Value| Envelope {
            protocol: ProtocolVersion {
                generation: 1,
                revision: 1,
            },
            daemon_generation: DaemonGeneration("current".into()),
            kind: EnvelopeKind::Request {
                request_id: usagi_core::infrastructure::ipc::RequestId(
                    usagi_core::domain::id::RequestId::new().as_str(),
                ),
                timeout_ms: None,
                body,
            },
        };
        let input = |input_operation: Option<serde_json::Value>| {
            json!({
                "kind": "terminal",
                "action": "input",
                "payload": {
                    "operation": "input",
                    "terminal": terminal_ref(),
                    "subscription": 1,
                    "input_seq": 0,
                    "input_operation": input_operation,
                    "bytes": [97],
                },
            })
        };

        let mut serve_ordinary =
            |request_id, _, hello: &ServerHello, _, _| dispatch(request_id, json!(null), hello);

        // Two independent connections from the same client incarnation.
        let mut owner = RecordingTerminal::default();
        for _ in 0..2 {
            let mut frames = Vec::new();
            write_json_frame(&mut frames, &Bootstrap::ClientHello(durable.clone()), 4096).unwrap();
            write_json_frame(&mut frames, &terminal_request(input(None)), 4096).unwrap();
            handle_connection_with_terminal_and(
                &mut Cursor::new(frames),
                &mut Vec::new(),
                &server(),
                &UnfencedConnection,
                &mut owner,
                &mut serve_ordinary,
            )
            .unwrap();
        }
        assert_eq!(owner.clients, vec![incarnation, incarnation]);

        // A peer without a canonical incarnation keeps working, but each of its
        // connections is a separate identity, so it can replay nothing.
        let mut legacy_owner = RecordingTerminal::default();
        for _ in 0..2 {
            let mut frames = Vec::new();
            write_json_frame(&mut frames, &hello(), 4096).unwrap();
            write_json_frame(&mut frames, &terminal_request(input(None)), 4096).unwrap();
            // Its ordinary, non-terminal traffic is served as before.
            write_json_frame(&mut frames, &request(), 4096).unwrap();
            handle_connection_with_terminal_and(
                &mut Cursor::new(frames),
                &mut Vec::new(),
                &server(),
                &UnfencedConnection,
                &mut legacy_owner,
                &mut serve_ordinary,
            )
            .unwrap();
        }
        assert_ne!(legacy_owner.clients[0], legacy_owner.clients[1]);

        // And it may not ask for cross-connection semantics it cannot be given:
        // the request is refused before the owner ever sees it.
        let mut refused = RecordingTerminal::default();
        let mut frames = Vec::new();
        write_json_frame(&mut frames, &hello(), 4096).unwrap();
        write_json_frame(
            &mut frames,
            &terminal_request(input(Some(json!(
                usagi_core::domain::id::OperationId::new().as_str()
            )))),
            4096,
        )
        .unwrap();
        let mut output = Vec::new();
        handle_connection_with_terminal_and(
            &mut Cursor::new(frames),
            &mut output,
            &server(),
            &UnfencedConnection,
            &mut refused,
            &mut serve_ordinary,
        )
        .unwrap();
        assert!(refused.clients.is_empty());
        let mut output = Cursor::new(output);
        let _ = read_json_frame::<Bootstrap>(&mut output, 4096).unwrap();
        let response = serde_json::to_value(
            read_json_frame::<Envelope>(&mut output, 4096)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(response["outcome"]["outcome"], "error");
        assert_eq!(response["outcome"]["value"]["code"], "unauthenticated");
        assert_eq!(response["outcome"]["value"]["side_effect"], "none");
    }

    fn terminal_ref() -> serde_json::Value {
        json!({
            "daemon_generation": usagi_core::domain::id::DaemonGeneration::new().as_str(),
            "terminal_id": usagi_core::domain::id::TerminalId::new().as_str(),
            "workspace_id": usagi_core::domain::id::WorkspaceId::new().as_str(),
            "session_id": null,
            "worktree_id": usagi_core::domain::id::WorktreeId::new().as_str(),
        })
    }

    #[test]
    fn terminal_server_preserves_a_session_dispatch_error() {
        let mut input = Vec::new();
        write_json_frame(&mut input, &hello(), 1024).unwrap();
        write_json_frame(&mut input, &request(), 1024).unwrap();
        let mut output = Vec::new();
        handle_connection_with_terminal_and(
            &mut Cursor::new(input),
            &mut output,
            &server(),
            &UnfencedConnection,
            &mut RecordingTerminal::default(),
            &mut |request_id, _, hello, _, _| Envelope {
                protocol: hello.protocol,
                daemon_generation: hello.daemon_generation.clone(),
                kind: EnvelopeKind::Response {
                    request_id,
                    outcome: ResponseOutcome::Error(ProtocolError::new(
                        ErrorCode::InvalidArgument,
                        "session branch already exists",
                    )),
                    body: json!(null),
                },
            },
        )
        .unwrap();
        let mut output = Cursor::new(output);
        let _ = read_json_frame::<Bootstrap>(&mut output, 1024).unwrap();
        let response = read_json_frame::<Envelope>(&mut output, 1024)
            .unwrap()
            .unwrap();
        assert!(matches!(
            response.kind,
            EnvelopeKind::Response {
                outcome: ResponseOutcome::Error(_),
                body,
                ..
            } if body.is_null()
        ));
    }

    #[test]
    fn the_negotiated_revision_advertises_and_selects_the_checkpoint_wire() {
        use crate::usecase::terminal::SnapshotWire;
        use usagi_core::infrastructure::ipc::{
            TERMINAL_CHECKPOINT_REVISION, TERMINAL_SCREEN_CHECKPOINT_CAPABILITY,
            TERMINAL_WIRE_GENERATION,
        };

        // The daemon advertises the checkpoint capability and revision 2.
        let server = server();
        assert!(
            server
                .capabilities
                .iter()
                .any(|capability| capability == TERMINAL_SCREEN_CHECKPOINT_CAPABILITY)
        );
        assert_eq!(
            server.supported_protocols,
            vec![ProtocolRange {
                generation: TERMINAL_WIRE_GENERATION,
                min_revision: 0,
                max_revision: TERMINAL_CHECKPOINT_REVISION,
            }]
        );

        // A revision 1 client keeps the raw tail; a revision 2 client gets the
        // checkpoint wire. Both are served by the same daemon.
        for (client_max, expected_revision, expected_wire) in [
            (1, 1, SnapshotWire::RawTail),
            (
                TERMINAL_CHECKPOINT_REVISION,
                TERMINAL_CHECKPOINT_REVISION,
                SnapshotWire::ScreenCheckpoint,
            ),
        ] {
            let mut client = client_hello();
            client.supported_protocols = vec![ProtocolRange {
                generation: TERMINAL_WIRE_GENERATION,
                min_revision: 0,
                max_revision: client_max,
            }];
            let mut request =
                terminal_request(usagi_core::domain::id::RequestId::new().to_string());
            request.protocol = ProtocolVersion {
                generation: TERMINAL_WIRE_GENERATION,
                revision: expected_revision,
            };
            let mut input = Vec::new();
            write_json_frame(&mut input, &Bootstrap::ClientHello(client), 1024).unwrap();
            write_json_frame(&mut input, &request, 1024).unwrap();
            let mut terminal = RecordingTerminal::default();
            handle_connection_with_terminal_and(
                &mut Cursor::new(input),
                &mut Vec::new(),
                &server,
                &UnfencedConnection,
                &mut terminal,
                &mut test_dispatch,
            )
            .unwrap();
            assert_eq!(terminal.wires, vec![expected_wire]);
        }
    }

    #[test]
    fn terminal_server_routes_success_errors_and_fences_before_effects() {
        let valid_id = usagi_core::domain::id::RequestId::new().to_string();
        let mut stale = terminal_request(usagi_core::domain::id::RequestId::new().to_string());
        stale.daemon_generation = DaemonGeneration("stale".into());
        let requests = [
            terminal_request(valid_id),
            terminal_request("not-a-resource-id".into()),
            stale,
        ];
        let mut input = Vec::new();
        write_json_frame(&mut input, &hello(), 1024).unwrap();
        for request in requests {
            write_json_frame(&mut input, &request, 1024).unwrap();
        }
        let mut terminal = RecordingTerminal::default();
        let mut output = Vec::new();
        handle_connection_with_terminal_and(
            &mut Cursor::new(input),
            &mut output,
            &server(),
            &UnfencedConnection,
            &mut terminal,
            &mut test_dispatch,
        )
        .unwrap();
        assert_eq!(terminal.requests, 1);
        assert_eq!(terminal.disconnects, 1);

        let mut output = Cursor::new(output);
        let _ = read_json_frame::<Bootstrap>(&mut output, 1024).unwrap();
        let replies = (0..3)
            .map(|_| {
                read_json_frame::<Envelope>(&mut output, 1024)
                    .unwrap()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            replies[0].kind,
            EnvelopeKind::Response {
                outcome: ResponseOutcome::Ok,
                ref body,
                ..
            } if body == &json!({"terminal": "handled"})
        ));
        assert!(replies[1..].iter().all(|reply| matches!(
            reply.kind,
            EnvelopeKind::Response {
                outcome: ResponseOutcome::Error(_),
                ..
            }
        )));

        let mut input = Vec::new();
        write_json_frame(&mut input, &hello(), 1024).unwrap();
        write_json_frame(
            &mut input,
            &terminal_request(usagi_core::domain::id::RequestId::new().to_string()),
            1024,
        )
        .unwrap();
        let mut terminal = RecordingTerminal {
            fail: true,
            ..RecordingTerminal::default()
        };
        let mut output = Vec::new();
        handle_connection_with_terminal_and(
            &mut Cursor::new(input),
            &mut output,
            &server(),
            &UnfencedConnection,
            &mut terminal,
            &mut test_dispatch,
        )
        .unwrap();
        let mut output = Cursor::new(output);
        let _ = read_json_frame::<Bootstrap>(&mut output, 1024).unwrap();
        let reply = read_json_frame::<Envelope>(&mut output, 1024)
            .unwrap()
            .unwrap();
        assert!(matches!(
            reply.kind,
            EnvelopeKind::Response {
                outcome: ResponseOutcome::Error(_),
                body,
                ..
            } if body.is_null()
        ));
    }

    #[test]
    fn terminal_server_disconnects_on_close_and_invalid_envelope() {
        let mut terminal = RecordingTerminal::default();
        handle_connection_with_terminal_and(
            &mut Cursor::new(Vec::<u8>::new()),
            &mut Vec::new(),
            &server(),
            &UnfencedConnection,
            &mut terminal,
            &mut test_dispatch,
        )
        .unwrap();
        assert_eq!(terminal.disconnects, 0);

        let mut input = Vec::new();
        write_json_frame(&mut input, &hello(), 1024).unwrap();
        let mut event = request();
        event.kind = EnvelopeKind::Event {
            subscription_id: usagi_core::infrastructure::ipc::SubscriptionId("s".into()),
            stream_ref: usagi_core::infrastructure::ipc::StreamRef {
                stream_id: usagi_core::infrastructure::ipc::StreamId("x".into()),
                epoch: "e".into(),
            },
            stream_sequence: 1,
            body: json!({}),
        };
        write_json_frame(&mut input, &event, 1024).unwrap();
        let error = handle_connection_with_terminal_and(
            &mut Cursor::new(input),
            &mut Vec::new(),
            &server(),
            &UnfencedConnection,
            &mut terminal,
            &mut test_dispatch,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(terminal.disconnects, 1);

        assert_eq!(event.kind_response(), (ResponseOutcome::Ok, json!(null)));
    }
    #[test]
    fn connection_rejects_normal_message_before_handshake() {
        let mut input = Vec::new();
        write_json_frame(&mut input, &request(), 1024).unwrap();
        assert_eq!(
            handle_connection(&mut Cursor::new(input), &mut Vec::new(), &server())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
    #[test]
    fn connection_returns_generation_error_with_request_id() {
        let mut input = Vec::new();
        write_json_frame(&mut input, &hello(), 1024).unwrap();
        let mut stale = request();
        stale.daemon_generation = DaemonGeneration("old".into());
        write_json_frame(&mut input, &stale, 1024).unwrap();
        let mut output = Vec::new();
        handle_connection(&mut Cursor::new(input), &mut output, &server()).unwrap();
        let mut output = Cursor::new(output);
        let _ = read_json_frame::<Bootstrap>(&mut output, 1024).unwrap();
        let response = read_json_frame::<Envelope>(&mut output, 1024)
            .unwrap()
            .unwrap();
        assert!(matches!(
            response.kind,
            EnvelopeKind::Response {
                outcome: usagi_core::infrastructure::ipc::ResponseOutcome::Error(_),
                ..
            }
        ));
    }

    #[test]
    fn handshake_handles_close_wrong_first_message_and_negotiation_error() {
        assert!(
            handshake(
                &mut Cursor::new(Vec::<u8>::new()),
                &mut Vec::new(),
                &server()
            )
            .unwrap()
            .is_none()
        );
        let mut wrong = Vec::new();
        write_json_frame(
            &mut wrong,
            &Bootstrap::Error(ProtocolError::new(ErrorCode::Internal, "x")),
            1024,
        )
        .unwrap();
        assert_eq!(
            handshake(&mut Cursor::new(wrong), &mut Vec::new(), &server())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        let mut bad = hello();
        if let Bootstrap::ClientHello(value) = &mut bad {
            value.required_capabilities.push("missing".into());
        }
        let mut input = Vec::new();
        write_json_frame(&mut input, &bad, 1024).unwrap();
        let mut output = Vec::new();
        assert!(
            handshake(&mut Cursor::new(input), &mut output, &server())
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            read_json_frame::<Bootstrap>(&mut Cursor::new(output), 1024).unwrap(),
            Some(Bootstrap::Error(_))
        ));
    }

    #[test]
    fn a_client_from_another_workspace_is_refused_before_any_request_reaches_a_runtime() {
        // The daemon advertises the fence so a workspace-bound client can require
        // it and refuse a daemon that would admit any workspace.
        assert!(
            server()
                .capabilities
                .contains(&WORKSPACE_FENCE_CAPABILITY.to_owned())
        );

        let mut elsewhere = client_hello();
        elsewhere.workspace = Some(ClientWorkspace::Bound {
            root: "/workspace/other".into(),
        });
        let mut input = Vec::new();
        write_json_frame(&mut input, &Bootstrap::ClientHello(elsewhere), 1024).unwrap();
        // A request follows the hello, as a real client's first RPC would.
        write_json_frame(&mut input, &request(), 1024).unwrap();

        let mut output = Vec::new();
        let mut terminal = RecordingTerminal::default();
        handle_connection_with_terminal_and(
            &mut Cursor::new(input),
            &mut output,
            &server(),
            &UnfencedConnection,
            &mut terminal,
            &mut test_dispatch,
        )
        .unwrap();

        // The connection ends at the refusal: nothing is dispatched, so no
        // session, scope, or PR inventory of this workspace is observed by a
        // client working in another one.
        assert_eq!(terminal.requests, 0);
        let mut replies = Cursor::new(output);
        let mut refused = None;
        if let Some(Bootstrap::Error(error)) =
            read_json_frame::<Bootstrap>(&mut replies, 1024).unwrap()
        {
            refused = Some(error);
        }
        let refusal = refused.expect("a foreign workspace is answered with a typed error frame");
        assert!(is_workspace_mismatch(&refusal));
        assert!(refusal.message.contains(TRUSTED_ROOT), "{refusal:?}");
        assert_eq!(
            read_json_frame::<Envelope>(&mut replies, 1024).unwrap(),
            None
        );
    }

    #[test]
    fn a_client_opening_another_workspace_is_refused_before_any_request_reaches_a_runtime() {
        // A TUI that opens a workspace declares that workspace, so a daemon that
        // serves a different one must refuse instead of answering the session list
        // it would then show under the opened workspace's name (#549). A
        // subdirectory of the served root is refused for the same reason: it is a
        // different workspace to open.
        for selected in [
            "/workspace/other",
            &format!("{TRUSTED_ROOT}/crates"),
            &format!("{TRUSTED_ROOT}/.usagi/sessions/issue-549"),
        ] {
            let mut elsewhere = client_hello();
            elsewhere.workspace = Some(ClientWorkspace::Selected {
                root: selected.to_string(),
            });
            let mut input = Vec::new();
            write_json_frame(&mut input, &Bootstrap::ClientHello(elsewhere), 1024).unwrap();
            write_json_frame(&mut input, &request(), 1024).unwrap();

            let mut output = Vec::new();
            let mut terminal = RecordingTerminal::default();
            handle_connection_with_terminal_and(
                &mut Cursor::new(input),
                &mut output,
                &server(),
                &UnfencedConnection,
                &mut terminal,
                &mut test_dispatch,
            )
            .unwrap();

            assert_eq!(terminal.requests, 0, "{selected}");
            let mut replies = Cursor::new(output);
            let mut refused = None;
            if let Some(Bootstrap::Error(error)) =
                read_json_frame::<Bootstrap>(&mut replies, 1024).unwrap()
            {
                refused = Some(error);
            }
            let refusal = refused.expect("an unserved workspace is answered with a typed error");
            assert!(is_workspace_mismatch(&refusal), "{selected}");
            assert!(refusal.message.contains(TRUSTED_ROOT), "{refusal:?}");
        }

        // Selecting the workspace this daemon serves reaches the runtime as usual.
        let mut here = client_hello();
        here.workspace = Some(ClientWorkspace::Selected {
            root: TRUSTED_ROOT.to_owned(),
        });
        let mut input = Vec::new();
        write_json_frame(&mut input, &Bootstrap::ClientHello(here), 1024).unwrap();
        write_json_frame(
            &mut input,
            &terminal_request(usagi_core::domain::id::RequestId::new().to_string()),
            1024,
        )
        .unwrap();
        let mut terminal = RecordingTerminal::default();
        handle_connection_with_terminal_and(
            &mut Cursor::new(input),
            &mut Vec::new(),
            &server(),
            &UnfencedConnection,
            &mut terminal,
            &mut test_dispatch,
        )
        .unwrap();
        assert_eq!(terminal.requests, 1);
    }

    #[test]
    fn connection_rejects_client_event_after_handshake() {
        let mut input = Vec::new();
        write_json_frame(&mut input, &hello(), 1024).unwrap();
        let event = Envelope {
            protocol: ProtocolVersion {
                generation: 1,
                revision: 1,
            },
            daemon_generation: DaemonGeneration("current".into()),
            kind: EnvelopeKind::Event {
                subscription_id: usagi_core::infrastructure::ipc::SubscriptionId("s".into()),
                stream_ref: usagi_core::infrastructure::ipc::StreamRef {
                    stream_id: usagi_core::infrastructure::ipc::StreamId("x".into()),
                    epoch: "e".into(),
                },
                stream_sequence: 1,
                body: json!({}),
            },
        };
        write_json_frame(&mut input, &event, 1024).unwrap();
        assert_eq!(
            handle_connection(&mut Cursor::new(input), &mut Vec::new(), &server())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn connection_accepts_clean_close_and_propagates_handshake_write_errors() {
        assert!(
            handle_connection(
                &mut Cursor::new(Vec::<u8>::new()),
                &mut Vec::new(),
                &server()
            )
            .is_ok()
        );
        let mut input = Vec::new();
        write_json_frame(&mut input, &hello(), 1024).unwrap();
        assert!(handshake(&mut Cursor::new(input), &mut BrokenWriter, &server()).is_err());
        let mut bad = hello();
        if let Bootstrap::ClientHello(value) = &mut bad {
            value.required_capabilities.push("missing".into());
        }
        let mut input = Vec::new();
        write_json_frame(&mut input, &bad, 1024).unwrap();
        assert!(handshake(&mut Cursor::new(input), &mut BrokenWriter, &server()).is_err());
        assert!(BrokenWriter.flush().is_ok());
    }

    #[test]
    fn dispatch_preserves_the_request_correlation_and_body() {
        let hello = handshake(
            &mut Cursor::new({
                let mut bytes = Vec::new();
                write_json_frame(&mut bytes, &hello(), 1024).unwrap();
                bytes
            }),
            &mut Vec::new(),
            &server(),
        )
        .unwrap()
        .unwrap();
        let reply = dispatch(
            usagi_core::infrastructure::ipc::RequestId("r".into()),
            json!({"x": 1}),
            &hello,
        );
        let _ = test_dispatch(
            usagi_core::infrastructure::ipc::RequestId("r2".into()),
            json!({"x": 2}),
            &hello,
            usagi_core::domain::id::ConnectionId::new(),
            usagi_core::domain::id::ClientId::new(),
        );
        assert!(matches!(
            reply.kind,
            EnvelopeKind::Response {
                request_id: usagi_core::infrastructure::ipc::RequestId(ref value),
                outcome: ResponseOutcome::Ok,
                body,
            } if value == "r" && body == json!({"x": 1})
        ));
    }

    #[test]
    fn dispatch_rejects_unimplemented_daemon_tool_families_without_echoing() {
        let hello = handshake(
            &mut Cursor::new({
                let mut bytes = Vec::new();
                write_json_frame(&mut bytes, &hello(), 1024).unwrap();
                bytes
            }),
            &mut Vec::new(),
            &server(),
        )
        .unwrap()
        .unwrap();
        for kind in ["dispatch_tool", "supervisor_tool"] {
            let reply = dispatch(
                usagi_core::infrastructure::ipc::RequestId("r".into()),
                json!({"kind": kind, "action": "placeholder", "secret": "do not echo"}),
                &hello,
            );
            assert!(matches!(
                reply.kind,
                EnvelopeKind::Response {
                    outcome: ResponseOutcome::Error(ProtocolError {
                        code: ErrorCode::InvalidArgument,
                        ref message,
                        ..
                    }),
                    body,
                    ..
                } if message.contains("not implemented") && body.is_null()
            ));
        }
    }

    #[test]
    fn dispatch_admits_agent_launch_with_its_producer_operation() {
        let hello = handshake(
            &mut Cursor::new({
                let mut bytes = Vec::new();
                write_json_frame(&mut bytes, &hello(), 1024).unwrap();
                bytes
            }),
            &mut Vec::new(),
            &server(),
        )
        .unwrap()
        .unwrap();
        let reply = dispatch(
            usagi_core::infrastructure::ipc::RequestId("r".into()),
            json!({"kind": "agent", "operation_id": "operation"}),
            &hello,
        );
        assert!(matches!(
            reply.kind,
            EnvelopeKind::Response {
                outcome: ResponseOutcome::Accepted { operation_id: OperationId(ref value), operation_revision: 1 },
                ..
            } if value == "operation"
        ));
    }
}

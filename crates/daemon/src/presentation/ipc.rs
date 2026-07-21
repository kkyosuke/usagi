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
    fn request(
        &mut self,
        connection: usagi_core::domain::id::ConnectionId,
        client: usagi_core::domain::id::ClientId,
        request_id: usagi_core::domain::id::RequestId,
        action: usagi_core::usecase::client::TerminalAction,
        payload: serde_json::Value,
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
    fn disconnect(&mut self, connection: usagi_core::domain::id::ConnectionId);
}

/// Complete a bootstrap handshake. No ordinary envelope is accepted before this succeeds.
pub fn handshake(
    reader: &mut dyn Read,
    writer: &mut dyn Write,
    server: &ServerProtocol,
) -> io::Result<Option<ServerHello>> {
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
            Ok(Some(reply))
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
            .filter(|kind| matches!(*kind, "session" | "agent" | "dispatch"))
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
pub fn handle_connection_with_terminal_and(
    reader: &mut dyn Read,
    writer: &mut dyn Write,
    server: &ServerProtocol,
    terminal: &mut dyn TerminalOwner,
    dispatch_request: &mut dyn FnMut(
        usagi_core::infrastructure::ipc::RequestId,
        serde_json::Value,
        &ServerHello,
        usagi_core::domain::id::ConnectionId,
        usagi_core::domain::id::ClientId,
    ) -> Envelope,
) -> io::Result<()> {
    let Some(hello) = handshake(reader, writer, server)? else {
        return Ok(());
    };
    let connection = usagi_core::domain::id::ConnectionId::new();
    let client = usagi_core::domain::id::ClientId::new();
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
                Err(ProtocolError::new(
                    ErrorCode::GenerationMismatch,
                    "request targets a different daemon generation",
                ))
            } else if let Ok(usagi_core::usecase::client::DaemonRequest::Terminal {
                action,
                payload,
            }) = serde_json::from_value(body.clone())
            {
                match usagi_core::domain::id::RequestId::parse(&request_id.0) {
                    Ok(owner_request_id) => terminal
                        .request(connection, client, owner_request_id, action, payload)
                        .map(ok_response),
                    Err(_) => Err(ProtocolError::new(
                        ErrorCode::InvalidArgument,
                        "terminal request_id must be a canonical resource ID",
                    )),
                }
            } else {
                let dispatched =
                    dispatch_request(request_id.clone(), body.clone(), &hello, connection, client);
                // Session, agent, and metrics dispatchers each own their
                // outcome.  Replacing a session error with `Ok(null)` makes a
                // client mistake the error body for a lifecycle snapshot.
                Ok(dispatched.kind_response())
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
    result
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
#[must_use]
pub fn server_protocol(
    daemon_generation: DaemonGeneration,
    connection_id: String,
    build: usagi_core::infrastructure::ipc::BuildIdentity,
) -> ServerProtocol {
    ServerProtocol {
        daemon_generation,
        connection_id: usagi_core::infrastructure::ipc::ConnectionId(connection_id),
        generation_role: usagi_core::infrastructure::ipc::GenerationRole::Active,
        supported_protocols: vec![usagi_core::infrastructure::ipc::ProtocolRange {
            generation: 1,
            min_revision: 0,
            max_revision: 1,
        }],
        capabilities: vec![
            "request.correlation.v1".into(),
            "pr.snapshot.v1".into(),
            "pr.subscription.v1".into(),
        ],
        build,
        limits: usagi_core::infrastructure::ipc::ProtocolLimits::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use usagi_core::infrastructure::ipc::{
        BuildIdentity, ClientHello, ClientId, ProtocolRange, ProtocolVersion, read_json_frame,
        write_json_frame,
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
    }
    impl TerminalOwner for RecordingTerminal {
        fn request(
            &mut self,
            _: usagi_core::domain::id::ConnectionId,
            _: usagi_core::domain::id::ClientId,
            _: usagi_core::domain::id::RequestId,
            _: usagi_core::usecase::client::TerminalAction,
            _: serde_json::Value,
        ) -> Result<serde_json::Value, ProtocolError> {
            self.requests += 1;
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

    fn server() -> ServerProtocol {
        server_protocol(
            DaemonGeneration("current".into()),
            "conn".into(),
            BuildIdentity {
                version: "1".into(),
                commit: "x".into(),
                target: "test".into(),
            },
        )
    }
    fn hello() -> Bootstrap {
        Bootstrap::ClientHello(ClientHello {
            client_id: ClientId("client".into()),
            connection_nonce: "n".into(),
            expected_daemon_generation: None,
            supported_protocols: vec![ProtocolRange {
                generation: 1,
                min_revision: 0,
                max_revision: 1,
            }],
            capabilities: vec![],
            required_capabilities: vec!["request.correlation.v1".into()],
            build: BuildIdentity {
                version: "other".into(),
                commit: "y".into(),
                target: "test".into(),
            },
        })
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

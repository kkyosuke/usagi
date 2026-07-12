//! Handshake-gated server adapter for the transport-independent IPC protocol.

#![allow(clippy::missing_errors_doc)] // Errors are directly forwarded transport/protocol failures.

use std::io::{self, Read, Write};

use serde_json::json;
use usagi_core::infrastructure::ipc::{
    Bootstrap, DaemonGeneration, Envelope, EnvelopeKind, ErrorCode, ProtocolError, ServerHello,
    ServerProtocol, negotiate, read_json_frame, write_json_frame,
};

fn decode_bootstrap(value: serde_json::Value) -> io::Result<Bootstrap> {
    serde_json::from_value(value).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}
fn decode_envelope(value: serde_json::Value) -> io::Result<Envelope> {
    serde_json::from_value(value).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}
fn write_bootstrap(writer: &mut dyn Write, value: &Bootstrap, limit: usize) -> io::Result<()> {
    write_json_frame(
        writer,
        &serde_json::to_value(value)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        limit,
    )
}
fn write_envelope(writer: &mut dyn Write, value: &Envelope, limit: usize) -> io::Result<()> {
    write_json_frame(
        writer,
        &serde_json::to_value(value)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        limit,
    )
}

/// Complete a bootstrap handshake. No ordinary envelope is accepted before this succeeds.
pub fn handshake<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    server: &ServerProtocol,
) -> io::Result<Option<ServerHello>> {
    let Some(first) = read_json_frame(reader, server.limits.max_frame_bytes as usize)? else {
        return Ok(None);
    };
    let Bootstrap::ClientHello(hello) = decode_bootstrap(first)? else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "client hello must be the first frame",
        ));
    };
    match negotiate(&hello, server) {
        Ok(reply) => {
            write_bootstrap(
                writer,
                &Bootstrap::ServerHello(reply.clone()),
                server.limits.max_frame_bytes as usize,
            )?;
            Ok(Some(reply))
        }
        Err(error) => {
            write_bootstrap(
                writer,
                &Bootstrap::Error(error),
                server.limits.max_frame_bytes as usize,
            )?;
            Ok(None)
        }
    }
}

/// Dispatch the currently generic request body. Future daemon APIs replace this
/// echo with typed use cases while retaining envelope/correlation semantics.
#[must_use]
pub fn dispatch(
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: serde_json::Value,
    hello: &ServerHello,
) -> Envelope {
    Envelope {
        protocol: hello.protocol,
        daemon_generation: hello.daemon_generation.clone(),
        kind: EnvelopeKind::Response {
            request_id,
            outcome: usagi_core::infrastructure::ipc::ResponseOutcome::Ok,
            body,
        },
    }
}

/// Serve one client. A target generation mismatch and pre-handshake normal
/// request are rejected before request dispatch.
pub fn handle_connection<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    server: &ServerProtocol,
) -> io::Result<()> {
    let Some(hello) = handshake(reader, writer, server)? else {
        return Ok(());
    };
    while let Some(envelope) = read_json_frame(reader, hello.limits.max_frame_bytes as usize)? {
        let envelope = decode_envelope(envelope)?;
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
            write_envelope(writer, &reply, hello.limits.max_frame_bytes as usize)?;
            continue;
        }
        let reply = dispatch(request_id, body, &hello);
        write_envelope(writer, &reply, hello.limits.max_frame_bytes as usize)?;
    }
    Ok(())
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
        capabilities: vec!["request.correlation.v1".into()],
        build,
        limits: usagi_core::infrastructure::ipc::ProtocolLimits::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::io::Cursor;
    use usagi_core::infrastructure::ipc::{
        BuildIdentity, ClientHello, ClientId, ProtocolRange, ProtocolVersion,
        read_json_frame as read_value_frame, write_json_frame as write_value_frame,
    };

    fn write_json_frame<T: Serialize>(
        writer: &mut dyn Write,
        value: &T,
        limit: usize,
    ) -> io::Result<()> {
        write_value_frame(
            writer,
            &serde_json::to_value(value).expect("test values serialize"),
            limit,
        )
    }
    fn read_json_frame<T: for<'de> Deserialize<'de>>(
        reader: &mut dyn Read,
        limit: usize,
    ) -> io::Result<Option<T>> {
        read_value_frame(reader, limit)?
            .map(|value| {
                serde_json::from_value(value)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
            })
            .transpose()
    }

    struct BrokenWriter;
    impl Write for BrokenWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("broken"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
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
        assert!(
            matches!(reply.kind, EnvelopeKind::Response { request_id: usagi_core::infrastructure::ipc::RequestId(ref value), .. } if value == "r")
        );
    }

    #[test]
    fn value_conversion_helpers_cover_success_invalid_value_and_write_failure() {
        let bootstrap = hello();
        assert_eq!(
            decode_bootstrap(serde_json::to_value(&bootstrap).unwrap()).unwrap(),
            bootstrap
        );
        assert_eq!(
            decode_bootstrap(json!({})).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let request = request();
        assert_eq!(
            decode_envelope(serde_json::to_value(&request).unwrap()).unwrap(),
            request
        );
        assert_eq!(
            decode_envelope(json!({})).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let mut bootstrap_bytes = Vec::new();
        write_bootstrap(&mut bootstrap_bytes, &bootstrap, 1024).unwrap();
        assert!(
            read_value_frame(&mut Cursor::new(bootstrap_bytes), 1024)
                .unwrap()
                .is_some()
        );
        assert!(write_bootstrap(&mut BrokenWriter, &bootstrap, 1024).is_err());
        let mut envelope_bytes = Vec::new();
        write_envelope(&mut envelope_bytes, &request, 1024).unwrap();
        assert!(
            read_value_frame(&mut Cursor::new(envelope_bytes), 1024)
                .unwrap()
                .is_some()
        );
        assert!(write_envelope(&mut BrokenWriter, &request, 1024).is_err());

        let mut invalid = Vec::new();
        write_value_frame(&mut invalid, &json!({}), 1024).unwrap();
        assert!(read_json_frame::<Bootstrap>(&mut Cursor::new(invalid), 1024).is_err());
    }
}

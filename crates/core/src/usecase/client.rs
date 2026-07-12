//! Surface-neutral daemon client port.
//!
//! Presentation surfaces submit only typed request bodies through this port.  In
//! particular, a connection failure is not permission to mutate local session
//! state or to allocate a local managed PTY.

use std::fmt;
use std::io::{Read, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::infrastructure::ipc::{
    Bootstrap, BuildIdentity, ClientHello, ClientId, DaemonGeneration, Envelope, EnvelopeKind,
    ErrorCode, ProtocolError, ProtocolRange, ProtocolVersion, ResponseOutcome, RetryMode,
    SideEffect, read_json_frame, write_json_frame,
};

/// A daemon request understood by every presentation surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DaemonRequest {
    /// A lifecycle mutation. `operation_id` makes accepted work discoverable
    /// after a client disconnects.
    Session {
        action: SessionAction,
        operation_id: String,
        payload: Value,
    },
    /// A terminal attach/resume/resync request addressed only by its stable ref.
    Terminal {
        action: TerminalAction,
        payload: Value,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionAction {
    Create,
    Remove,
    Setup,
    Prompt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalAction {
    Attach,
    Resume,
    Resync,
    Input,
    Resize,
}

/// The result exposed to CLI and MCP adapters.
#[derive(Debug, Clone, PartialEq)]
pub enum DaemonReply {
    Ok(Value),
    Accepted { operation_id: String, revision: u64 },
}

/// Typed daemon failure.  Surfaces may render its safe details, but must not
/// infer that a local fallback is safe.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientError {
    Protocol(ProtocolError),
    Unavailable(String),
}

impl ClientError {
    #[must_use]
    pub fn retry_mode(&self) -> RetryMode {
        match self {
            Self::Protocol(error) => error.retry_mode,
            Self::Unavailable(_) => RetryMode::Reconnect,
        }
    }

    #[must_use]
    pub fn side_effect(&self) -> SideEffect {
        match self {
            Self::Protocol(error) => error.side_effect,
            Self::Unavailable(_) => SideEffect::PartialOrUnknown,
        }
    }

    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Protocol(error) => error.code,
            Self::Unavailable(_) => ErrorCode::Unavailable,
        }
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol(error) => write!(f, "{:?}: {}", error.code, error.message),
            Self::Unavailable(message) => write!(f, "Unavailable: {message}"),
        }
    }
}
impl std::error::Error for ClientError {}

/// Common port implemented by the composition root's Unix IPC client.
pub trait DaemonClient {
    /// Submit one request. Implementations preserve correlation and never
    /// substitute a local managed-session implementation when this fails.
    ///
    /// # Errors
    ///
    /// Returns a typed daemon or transport failure without attempting a local
    /// managed-session fallback.
    fn request(&mut self, request: DaemonRequest) -> Result<DaemonReply, ClientError>;
}

/// A synchronous framed implementation of [`DaemonClient`].  Unix socket
/// discovery and autospawn stay at the composition root; this type works over
/// any injected byte stream and is therefore usable in black-box tests too.
pub struct IpcClient<S> {
    stream: S,
    protocol: ProtocolVersion,
    daemon_generation: DaemonGeneration,
    next_request: u64,
    policy: ClientPolicy,
}

impl<S: Read + Write> IpcClient<S> {
    /// Performs the mandatory hello handshake before returning a usable client.
    ///
    /// # Errors
    ///
    /// Returns a typed protocol error from the peer, or an unavailable error
    /// when the byte stream cannot complete the handshake.
    pub fn connect(
        mut stream: S,
        client_id: String,
        connection_nonce: String,
        policy: ClientPolicy,
    ) -> Result<Self, ClientError> {
        let hello = Bootstrap::ClientHello(ClientHello {
            client_id: ClientId(client_id),
            connection_nonce,
            expected_daemon_generation: None,
            supported_protocols: vec![ProtocolRange {
                generation: 1,
                min_revision: 0,
                max_revision: 1,
            }],
            capabilities: vec![],
            required_capabilities: vec!["request.correlation.v1".into()],
            build: BuildIdentity {
                version: env!("CARGO_PKG_VERSION").into(),
                commit: "unknown".into(),
                target: std::env::consts::ARCH.into(),
            },
        });
        write_json_frame(&mut stream, &hello, 1_048_576)
            .map_err(|error| ClientError::Unavailable(error.to_string()))?;
        match read_json_frame::<Bootstrap>(&mut stream, 1_048_576)
            .map_err(|error| ClientError::Unavailable(error.to_string()))?
        {
            Some(Bootstrap::ServerHello(hello)) => Ok(Self {
                stream,
                protocol: hello.protocol,
                daemon_generation: hello.daemon_generation,
                next_request: 0,
                policy,
            }),
            Some(Bootstrap::Error(error)) => Err(ClientError::Protocol(error)),
            Some(Bootstrap::ClientHello(_)) | None => Err(ClientError::Unavailable(
                "daemon closed before a server hello".into(),
            )),
        }
    }
}

impl<S: Read + Write> DaemonClient for IpcClient<S> {
    fn request(&mut self, request: DaemonRequest) -> Result<DaemonReply, ClientError> {
        self.next_request += 1;
        let request_id = crate::infrastructure::ipc::RequestId(self.next_request.to_string());
        let envelope = Envelope {
            protocol: self.protocol,
            daemon_generation: self.daemon_generation.clone(),
            kind: EnvelopeKind::Request {
                request_id: request_id.clone(),
                timeout_ms: Some(self.policy.timeout_ms),
                body: serde_json::to_value(request).expect("daemon request serializes"),
            },
        };
        write_json_frame(&mut self.stream, &envelope, 1_048_576)
            .map_err(|error| ClientError::Unavailable(error.to_string()))?;
        loop {
            let response = read_json_frame::<Envelope>(&mut self.stream, 1_048_576)
                .map_err(|error| ClientError::Unavailable(error.to_string()))?
                .ok_or_else(|| {
                    ClientError::Unavailable("daemon closed while awaiting response".into())
                })?;
            if let EnvelopeKind::Response {
                request_id: received,
                outcome,
                body,
            } = response.kind
            {
                if received != request_id {
                    continue;
                }
                return match outcome {
                    ResponseOutcome::Ok => Ok(DaemonReply::Ok(body)),
                    ResponseOutcome::Accepted {
                        operation_id,
                        operation_revision,
                    } => Ok(DaemonReply::Accepted {
                        operation_id: operation_id.0,
                        revision: operation_revision,
                    }),
                    ResponseOutcome::Error(error) => Err(ClientError::Protocol(error)),
                };
            }
        }
    }
}

/// Per-surface timeout/reconnect policy. Retry is intentionally explicit: a
/// mutation may only be retried with its original request/operation identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientPolicy {
    pub timeout_ms: u64,
    pub reconnect_attempts: u8,
}

impl ClientPolicy {
    #[must_use]
    pub const fn tui() -> Self {
        Self {
            timeout_ms: 2_000,
            reconnect_attempts: 3,
        }
    }
    #[must_use]
    pub const fn cli() -> Self {
        Self {
            timeout_ms: 10_000,
            reconnect_attempts: 1,
        }
    }
    #[must_use]
    pub const fn mcp() -> Self {
        Self {
            timeout_ms: 30_000,
            reconnect_attempts: 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Cursor};

    struct Scripted {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
    }
    impl Read for Scripted {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.input.read(buf)
        }
    }
    impl Write for Scripted {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn scripted(reply: ResponseOutcome) -> Scripted {
        let protocol = ProtocolVersion {
            generation: 1,
            revision: 1,
        };
        let generation = DaemonGeneration("daemon".into());
        let hello = Bootstrap::ServerHello(crate::infrastructure::ipc::ServerHello {
            connection_nonce: "nonce".into(),
            connection_id: crate::infrastructure::ipc::ConnectionId("connection".into()),
            daemon_generation: generation.clone(),
            generation_role: crate::infrastructure::ipc::GenerationRole::Active,
            protocol,
            capabilities: vec![],
            build: BuildIdentity {
                version: "test".into(),
                commit: "test".into(),
                target: "test".into(),
            },
            limits: crate::infrastructure::ipc::ProtocolLimits::default(),
        });
        let response = Envelope {
            protocol,
            daemon_generation: generation,
            kind: EnvelopeKind::Response {
                request_id: crate::infrastructure::ipc::RequestId("1".into()),
                outcome: reply,
                body: serde_json::json!({"ok":true}),
            },
        };
        let mut input = Vec::new();
        write_json_frame(&mut input, &hello, 1_048_576).unwrap();
        write_json_frame(&mut input, &response, 1_048_576).unwrap();
        Scripted {
            input: Cursor::new(input),
            output: vec![],
        }
    }

    #[test]
    fn unavailable_is_reconnectable_but_has_unknown_side_effect() {
        let error = ClientError::Unavailable("daemon is absent".into());
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.retry_mode(), RetryMode::Reconnect);
        assert_eq!(error.side_effect(), SideEffect::PartialOrUnknown);
    }

    #[test]
    fn policies_are_surface_specific() {
        assert!(ClientPolicy::tui().timeout_ms < ClientPolicy::cli().timeout_ms);
        assert!(ClientPolicy::mcp().timeout_ms > ClientPolicy::cli().timeout_ms);
    }

    #[test]
    fn client_handshakes_and_preserves_accepted_operation() {
        let stream = scripted(ResponseOutcome::Accepted {
            operation_id: crate::infrastructure::ipc::OperationId("op".into()),
            operation_revision: 7,
        });
        let mut client =
            IpcClient::connect(stream, "client".into(), "nonce".into(), ClientPolicy::cli())
                .unwrap();
        assert_eq!(
            client
                .request(DaemonRequest::Session {
                    action: SessionAction::Create,
                    operation_id: "op".into(),
                    payload: serde_json::json!({})
                })
                .unwrap(),
            DaemonReply::Accepted {
                operation_id: "op".into(),
                revision: 7
            }
        );
    }
}

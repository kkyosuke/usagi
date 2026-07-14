//! Surface-neutral daemon client port.
//!
//! Presentation surfaces submit only typed request bodies through this port.  In
//! particular, a connection failure is not permission to mutate local session
//! state or to allocate a local managed PTY.

#![coverage(off)] // Transport boundary behavior is exercised through injected stream tests.

use std::fmt;
use std::io::{Read, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::agent::AgentProfileId;
use crate::domain::id::{SessionId, TerminalRef, WorkspaceId};
use crate::domain::terminal_launch::{TerminalLaunchRequest, TerminalProfileId};
use crate::infrastructure::ipc::{
    Bootstrap, BuildIdentity, ClientHello, ClientId, DaemonGeneration, Envelope, EnvelopeKind,
    ErrorCode, ProtocolError, ProtocolRange, ProtocolVersion, ResponseOutcome, RetryMode,
    SideEffect, read_json_frame, write_json_frame,
};

/// A daemon request understood by every presentation surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DaemonRequest {
    /// Manage a daemon-owned periodic metrics subscription.  Metrics are
    /// observational only: they never authorize a client-side fallback.
    Metrics { action: MetricsAction },
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
    /// Start an Agent owned by the daemon. The daemon resolves the selected
    /// session's worktree and its default profile; clients never send argv,
    /// environment values, or a local process fallback.
    Agent {
        operation_id: String,
        intent: AgentLaunchIntent,
    },
}

/// Control vocabulary for the daemon metrics stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricsAction {
    Subscribe,
    Unsubscribe,
    Snapshot,
}

/// A deliberately small, versioned snapshot emitted by the daemon.  Counters
/// are process-local observations, not durable state or a control surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonMetrics {
    pub schema_version: u16,
    pub sampled_at_ms: u64,
    /// Daemon process CPU usage since the previous sample, in hundredths of a percent.
    #[serde(default)]
    pub cpu_percent_hundredths: u32,
    /// Daemon process peak resident memory, in bytes.
    #[serde(default)]
    pub resident_memory_bytes: u64,
    pub active_subscribers: u32,
    pub dropped_updates: u64,
}

/// Product-neutral Agent launch intent sent by a TUI client.
///
/// The stable session identity is enough for the daemon to resolve its durable
/// worktree scope. An omitted profile deliberately delegates selection to the
/// daemon's default policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLaunchIntent {
    pub workspace: WorkspaceId,
    pub session: SessionId,
    pub profile: Option<AgentProfileId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionAction {
    Create,
    Remove,
    List,
    Overview,
    Setup,
    Prompt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalAction {
    /// Reserve and spawn a daemon-owned generic terminal.  The payload is a
    /// [`TerminalLaunchIntent`], never a command line or environment.
    Launch,
    Inventory,
    Attach,
    Resume,
    Resync,
    Input,
    Resize,
    Detach,
}

/// Product-neutral generic terminal launch vocabulary.  It deliberately
/// serializes only a stable profile selector, a fully fenced scope and screen
/// geometry; process provision remains daemon-private.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalLaunchIntent {
    pub request: TerminalLaunchRequest,
    pub geometry: TerminalGeometry,
}

/// Geometry supplied by a terminal client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalGeometry {
    pub cols: u16,
    pub rows: u16,
}

/// Typed terminal command payloads.  Keeping these vocabulary types next to
/// the shared daemon client prevents UI/CLI adapters from inventing local PTY
/// fallback fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum TerminalRequest {
    Launch {
        intent: TerminalLaunchIntent,
    },
    Inventory {
        scope: TerminalLaunchRequest,
    },
    Attach {
        terminal: TerminalRef,
    },
    Resume {
        terminal: TerminalRef,
        after_offset: u64,
    },
    Resync {
        terminal: TerminalRef,
    },
    Input {
        terminal: TerminalRef,
        subscription: u64,
        input_seq: u64,
        bytes: Vec<u8>,
    },
    Resize {
        terminal: TerminalRef,
        geometry: TerminalGeometry,
    },
    Detach {
        terminal: TerminalRef,
        subscription: u64,
    },
}

/// Re-exported selection type makes callers name the only accepted launch
/// selector, rather than constructing an untyped JSON payload.
pub type TerminalProfileSelection = TerminalProfileId;

/// The result exposed to CLI and MCP adapters.
#[derive(Debug, Clone, PartialEq)]
pub enum DaemonReply {
    Ok(Value),
    Accepted {
        operation_id: String,
        revision: u64,
        /// Admission payload. Agent admission carries the fenced terminal that
        /// was spawned by the daemon; clients must not rediscover it by name.
        body: Value,
    },
}

/// Typed daemon failure.  Surfaces may render its safe details, but must not
/// infer that a local fallback is safe.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientError {
    Protocol(ProtocolError),
    Unavailable(String),
    /// A daemon lifecycle transition could not safely establish a verified
    /// endpoint. Callers must not replace it with a local implementation.
    Lifecycle(String),
}

impl ClientError {
    #[must_use]
    #[coverage(off)]
    pub fn retry_mode(&self) -> RetryMode {
        match self {
            Self::Protocol(error) => error.retry_mode,
            Self::Unavailable(_) | Self::Lifecycle(_) => RetryMode::Reconnect,
        }
    }

    #[must_use]
    #[coverage(off)]
    pub fn side_effect(&self) -> SideEffect {
        match self {
            Self::Protocol(error) => error.side_effect,
            Self::Unavailable(_) | Self::Lifecycle(_) => SideEffect::PartialOrUnknown,
        }
    }

    #[must_use]
    #[coverage(off)]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Protocol(error) => error.code,
            Self::Unavailable(_) | Self::Lifecycle(_) => ErrorCode::Unavailable,
        }
    }
}

impl fmt::Display for ClientError {
    #[coverage(off)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol(error) => write!(f, "{:?}: {}", error.code, error.message),
            Self::Unavailable(message) => write!(f, "Unavailable: {message}"),
            Self::Lifecycle(message) => write!(f, "Lifecycle: {message}"),
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
    server_build: BuildIdentity,
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
    #[coverage(off)]
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
                server_build: hello.build,
                next_request: 0,
                policy,
            }),
            Some(Bootstrap::Error(error)) => Err(ClientError::Protocol(error)),
            Some(Bootstrap::ClientHello(_)) | None => Err(ClientError::Unavailable(
                "daemon closed before a server hello".into(),
            )),
        }
    }

    /// Returns the build identity advertised by the daemon during the mandatory
    /// handshake. Composition roots use this only to decide whether their
    /// running binary must replace an older daemon; it is not protocol
    /// compatibility negotiation.
    #[must_use]
    #[coverage(off)]
    pub fn server_build(&self) -> &BuildIdentity {
        &self.server_build
    }
}

impl<S: Read + Write> DaemonClient for IpcClient<S> {
    #[coverage(off)]
    fn request(&mut self, request: DaemonRequest) -> Result<DaemonReply, ClientError> {
        self.next_request += 1;
        // Terminal owners use this correlation ID as part of their input
        // dedupe fence, so it must satisfy the canonical resource-ID contract
        // they validate at the server boundary.  Other request kinds retain
        // the compact per-connection sequence used by their response cache.
        let request_id = if matches!(&request, DaemonRequest::Terminal { .. }) {
            crate::infrastructure::ipc::RequestId(format!(
                "00000000-0000-4000-8000-{:012x}",
                self.next_request
            ))
        } else {
            crate::infrastructure::ipc::RequestId(self.next_request.to_string())
        };
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
                        body,
                    }),
                    ResponseOutcome::Error(error) => Err(ClientError::Protocol(error)),
                };
            }
        }
    }
}

#[cfg(test)]
mod metrics_schema_tests {
    use super::{DaemonMetrics, DaemonRequest, MetricsAction};

    #[test]
    fn metrics_schema_is_tagged_and_versioned() {
        assert_eq!(
            serde_json::to_value(DaemonRequest::Metrics {
                action: MetricsAction::Subscribe,
            })
            .unwrap(),
            serde_json::json!({"kind": "metrics", "action": "subscribe"})
        );
        let snapshot: DaemonMetrics = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "sampled_at_ms": 42,
            "cpu_percent_hundredths": 123,
            "resident_memory_bytes": 456,
            "active_subscribers": 2,
            "dropped_updates": 3
        }))
        .unwrap();
        assert_eq!(snapshot.schema_version, 1);
        assert_eq!(snapshot.cpu_percent_hundredths, 123);
        assert_eq!(snapshot.resident_memory_bytes, 456);

        let legacy_snapshot: DaemonMetrics = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "sampled_at_ms": 42,
            "active_subscribers": 2,
            "dropped_updates": 3
        }))
        .unwrap();
        assert_eq!(legacy_snapshot.cpu_percent_hundredths, 0);
        assert_eq!(legacy_snapshot.resident_memory_bytes, 0);
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
    #[coverage(off)]
    pub const fn tui() -> Self {
        Self {
            timeout_ms: 2_000,
            reconnect_attempts: 3,
        }
    }
    #[must_use]
    #[coverage(off)]
    pub const fn cli() -> Self {
        Self {
            timeout_ms: 10_000,
            reconnect_attempts: 1,
        }
    }
    #[must_use]
    #[coverage(off)]
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

    struct Broken;
    impl Read for Broken {
        #[coverage(off)]
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("read failed"))
        }
    }

    struct ReadFails {
        output: Vec<u8>,
    }
    impl Read for ReadFails {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("read failed"))
        }
    }
    impl Write for ReadFails {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }
        #[coverage(off)]
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Write for Broken {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("write failed"))
        }
        #[coverage(off)]
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn scripted(reply: ResponseOutcome, request_id: &str) -> Scripted {
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
            daemon_generation: generation.clone(),
            kind: EnvelopeKind::Response {
                request_id: crate::infrastructure::ipc::RequestId(request_id.into()),
                outcome: reply,
                body: serde_json::json!({"ok":true}),
            },
        };
        let mut input = Vec::new();
        write_json_frame(&mut input, &hello, 1_048_576).unwrap();
        let event = Envelope {
            protocol,
            daemon_generation: generation.clone(),
            kind: EnvelopeKind::Event {
                subscription_id: crate::infrastructure::ipc::SubscriptionId("s".into()),
                stream_ref: crate::infrastructure::ipc::StreamRef {
                    stream_id: crate::infrastructure::ipc::StreamId("stream".into()),
                    epoch: "epoch".into(),
                },
                stream_sequence: 1,
                body: serde_json::json!({}),
            },
        };
        write_json_frame(&mut input, &event, 1_048_576).unwrap();
        let unrelated = Envelope {
            protocol,
            daemon_generation: generation.clone(),
            kind: EnvelopeKind::Response {
                request_id: crate::infrastructure::ipc::RequestId("other".into()),
                outcome: ResponseOutcome::Ok,
                body: serde_json::json!({}),
            },
        };
        write_json_frame(&mut input, &unrelated, 1_048_576).unwrap();
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
        let stream = scripted(
            ResponseOutcome::Accepted {
                operation_id: crate::infrastructure::ipc::OperationId("op".into()),
                operation_revision: 7,
            },
            "1",
        );
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
                revision: 7,
                body: serde_json::json!({"ok": true}),
            }
        );
    }

    #[test]
    fn protocol_errors_are_rendered_and_keep_their_retry_contract() {
        let mut error = ProtocolError::new(ErrorCode::OwnershipUnknown, "owner vanished");
        error.retry_mode = RetryMode::Manual;
        error.side_effect = SideEffect::Applied;
        let error = ClientError::Protocol(error);
        assert_eq!(error.code(), ErrorCode::OwnershipUnknown);
        assert_eq!(error.retry_mode(), RetryMode::Manual);
        assert_eq!(error.side_effect(), SideEffect::Applied);
        assert!(error.to_string().contains("owner vanished"));
    }

    #[test]
    #[coverage(off)]
    fn client_returns_ok_and_protocol_error_replies() {
        for reply in [
            ResponseOutcome::Ok,
            ResponseOutcome::Error(ProtocolError::new(ErrorCode::Busy, "busy")),
        ] {
            let stream = scripted(reply, "00000000-0000-4000-8000-000000000001");
            let mut client =
                IpcClient::connect(stream, "client".into(), "nonce".into(), ClientPolicy::cli())
                    .unwrap();
            let result = client.request(DaemonRequest::Terminal {
                action: TerminalAction::Resync,
                payload: serde_json::json!({}),
            });
            match result {
                Ok(DaemonReply::Ok(value)) => assert_eq!(value["ok"], true),
                Err(ClientError::Protocol(error)) => assert_eq!(error.code, ErrorCode::Busy),
                other => panic!("unexpected response: {other:?}"),
            }
        }
    }

    #[test]
    fn client_rejects_error_and_missing_handshakes() {
        let protocol_error = ProtocolError::new(ErrorCode::ProtocolMismatch, "nope");
        let mut bytes = Vec::new();
        write_json_frame(&mut bytes, &Bootstrap::Error(protocol_error), 1_048_576).unwrap();
        assert!(matches!(
            IpcClient::connect(
                Scripted {
                    input: Cursor::new(bytes),
                    output: vec![]
                },
                "c".into(),
                "n".into(),
                ClientPolicy::tui()
            ),
            Err(ClientError::Protocol(_))
        ));
        assert!(matches!(
            IpcClient::connect(
                Scripted {
                    input: Cursor::new(vec![]),
                    output: vec![]
                },
                "c".into(),
                "n".into(),
                ClientPolicy::tui()
            ),
            Err(ClientError::Unavailable(_))
        ));
        assert!(matches!(
            IpcClient::connect(Broken, "c".into(), "n".into(), ClientPolicy::tui()),
            Err(ClientError::Unavailable(_))
        ));
        assert!(matches!(
            IpcClient::connect(
                ReadFails { output: vec![] },
                "c".into(),
                "n".into(),
                ClientPolicy::tui()
            ),
            Err(ClientError::Unavailable(_))
        ));
    }

    #[test]
    fn request_maps_transport_failures_to_unavailable() {
        let protocol = ProtocolVersion {
            generation: 1,
            revision: 1,
        };
        let request = DaemonRequest::Terminal {
            action: TerminalAction::Attach,
            payload: serde_json::json!({}),
        };
        let server_build = BuildIdentity {
            version: "test".into(),
            commit: "unknown".into(),
            target: "test".into(),
        };
        let mut broken = IpcClient {
            stream: Broken,
            protocol,
            daemon_generation: DaemonGeneration("d".into()),
            server_build: server_build.clone(),
            next_request: 0,
            policy: ClientPolicy::tui(),
        };
        assert!(matches!(
            broken.request(request.clone()),
            Err(ClientError::Unavailable(_))
        ));
        let mut closed = IpcClient {
            stream: Scripted {
                input: Cursor::new(vec![]),
                output: vec![],
            },
            protocol,
            daemon_generation: DaemonGeneration("d".into()),
            server_build: server_build.clone(),
            next_request: 0,
            policy: ClientPolicy::tui(),
        };
        assert!(matches!(
            closed.request(request),
            Err(ClientError::Unavailable(_))
        ));
        let mut read_fails = IpcClient {
            stream: ReadFails { output: vec![] },
            protocol,
            daemon_generation: DaemonGeneration("d".into()),
            server_build,
            next_request: 0,
            policy: ClientPolicy::tui(),
        };
        assert!(matches!(
            read_fails.request(DaemonRequest::Terminal {
                action: TerminalAction::Attach,
                payload: serde_json::json!({}),
            }),
            Err(ClientError::Unavailable(_))
        ));
        assert!(
            Scripted {
                input: Cursor::new(vec![]),
                output: vec![]
            }
            .flush()
            .is_ok()
        );
    }
}

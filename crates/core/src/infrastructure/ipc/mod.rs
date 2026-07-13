//! Transport-independent IPC protocol vocabulary and its bounded JSON framing.
//!
//! A transport supplies bytes; this module supplies the protocol contract.  In
//! particular, build identity is diagnostic only: compatibility is negotiated
//! from protocol generation/revision and capabilities.

#![allow(clippy::missing_errors_doc)] // All public codec errors are transport/protocol errors documented above.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Default and hard limit for one JSON frame (one MiB).
pub const DEFAULT_MAX_FRAME_BYTES: usize = 1024 * 1024;
/// The largest logical snapshot permitted by the protocol (sixteen MiB).
pub const HARD_MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;

macro_rules! string_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);
    };
}
string_id!(ClientId, "A stable client-process identifier.");
string_id!(RequestId, "A request correlation identifier.");
string_id!(
    OperationId,
    "A producer-issued durable operation identifier."
);
string_id!(DaemonGeneration, "The fenced daemon-generation identifier.");
string_id!(ConnectionId, "A connection-local routing identifier.");
string_id!(
    SubscriptionId,
    "A connection-local subscription identifier."
);
string_id!(StreamId, "A durable stream identifier.");

/// Build metadata for diagnostics; it never decides compatibility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildIdentity {
    pub version: String,
    pub commit: String,
    pub target: String,
}

/// A generation-specific range of revisions a peer supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolRange {
    pub generation: u16,
    pub min_revision: u16,
    pub max_revision: u16,
}

/// The negotiated protocol version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolVersion {
    pub generation: u16,
    pub revision: u16,
}

/// Limits advertised by the server after negotiation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolLimits {
    pub max_frame_bytes: u32,
    pub max_in_flight_requests: u16,
    pub max_input_batch_bytes: u32,
    pub response_cache_window_ms: u64,
    pub operation_admission_window_ms: u64,
    pub max_future_skew_ms: u64,
}

impl Default for ProtocolLimits {
    #[coverage(off)]
    fn default() -> Self {
        Self {
            max_frame_bytes: 1_048_576,
            max_in_flight_requests: 128,
            max_input_batch_bytes: 65_536,
            response_cache_window_ms: 86_400_000,
            operation_admission_window_ms: 86_400_000,
            max_future_skew_ms: 300_000,
        }
    }
}

/// The first frame sent by a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    pub client_id: ClientId,
    pub connection_nonce: String,
    pub expected_daemon_generation: Option<DaemonGeneration>,
    pub supported_protocols: Vec<ProtocolRange>,
    pub capabilities: Vec<String>,
    pub required_capabilities: Vec<String>,
    pub build: BuildIdentity,
}

/// The server's successful handshake result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerHello {
    pub connection_nonce: String,
    pub connection_id: ConnectionId,
    pub daemon_generation: DaemonGeneration,
    pub generation_role: GenerationRole,
    pub protocol: ProtocolVersion,
    pub capabilities: Vec<String>,
    pub build: BuildIdentity,
    pub limits: ProtocolLimits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationRole {
    Active,
    Draining,
}

/// Bootstrap messages are deliberately separate from post-handshake envelopes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Bootstrap {
    ClientHello(ClientHello),
    ServerHello(ServerHello),
    Error(ProtocolError),
}

/// A stream incarnation. `epoch` changes when ownership/generation changes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StreamRef {
    pub stream_id: StreamId,
    pub epoch: String,
}

/// Resume data keeps delivery order and resource cursors intentionally separate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeToken {
    pub stream_ref: StreamRef,
    pub after_sequence: Option<u64>,
    pub resource_revision: Option<u64>,
    pub terminal_output_offset: Option<u64>,
}

/// All ordinary traffic after a successful handshake.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub protocol: ProtocolVersion,
    pub daemon_generation: DaemonGeneration,
    #[serde(flatten)]
    pub kind: EnvelopeKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EnvelopeKind {
    Request {
        request_id: RequestId,
        timeout_ms: Option<u64>,
        body: Value,
    },
    Response {
        request_id: RequestId,
        outcome: ResponseOutcome,
        body: Value,
    },
    Event {
        subscription_id: SubscriptionId,
        stream_ref: StreamRef,
        stream_sequence: u64,
        body: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "value", rename_all = "snake_case")]
pub enum ResponseOutcome {
    Ok,
    Accepted {
        operation_id: OperationId,
        operation_revision: u64,
    },
    Error(ProtocolError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    ProtocolMismatch,
    CapabilityMissing,
    GenerationMismatch,
    Unauthenticated,
    PermissionDenied,
    InvalidArgument,
    NotFound,
    StaleTarget,
    GenerationRolledOver,
    RevisionConflict,
    IdempotencyConflict,
    IdempotencyExpired,
    ResourceExhausted,
    Backpressure,
    Busy,
    DeadlineExceeded,
    Cancelled,
    OwnershipUnknown,
    Unavailable,
    Internal,
    ResyncRequired,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryMode {
    Never,
    SameRequest,
    SameOperation,
    Reconnect,
    Resync,
    Manual,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffect {
    None,
    OperationAccepted,
    Applied,
    PartialOrUnknown,
}

/// A stable, safe-to-display machine error. `details` must not contain OS errors or secrets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProtocolError {
    pub code: ErrorCode,
    pub message: String,
    pub retry_mode: RetryMode,
    pub side_effect: SideEffect,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    pub error_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_daemon_generation: Option<DaemonGeneration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_protocol: Option<ProtocolVersion>,
}

impl ProtocolError {
    #[must_use]
    #[coverage(off)]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        let retry_mode = match code {
            ErrorCode::ResyncRequired => RetryMode::Resync,
            ErrorCode::Unavailable => RetryMode::Reconnect,
            ErrorCode::DeadlineExceeded => RetryMode::SameRequest,
            _ => RetryMode::Never,
        };
        Self {
            code,
            message: message.into(),
            retry_mode,
            side_effect: SideEffect::None,
            details: None,
            error_id: "protocol".into(),
            current_daemon_generation: None,
            current_protocol: None,
        }
    }
}

/// Server policy used by the pure handshake negotiator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerProtocol {
    pub daemon_generation: DaemonGeneration,
    pub connection_id: ConnectionId,
    pub generation_role: GenerationRole,
    pub supported_protocols: Vec<ProtocolRange>,
    pub capabilities: Vec<String>,
    pub build: BuildIdentity,
    pub limits: ProtocolLimits,
}

/// Negotiate version/capabilities, rejecting mismatched generation before normal traffic.
#[coverage(off)]
pub fn negotiate(
    hello: &ClientHello,
    server: &ServerProtocol,
) -> Result<ServerHello, ProtocolError> {
    if hello
        .expected_daemon_generation
        .as_ref()
        .is_some_and(|g| g != &server.daemon_generation)
    {
        let mut error = ProtocolError::new(
            ErrorCode::GenerationMismatch,
            "target daemon generation does not match",
        );
        error.current_daemon_generation = Some(server.daemon_generation.clone());
        return Err(error);
    }
    let protocol = hello
        .supported_protocols
        .iter()
        .flat_map(|client| {
            server.supported_protocols.iter().filter_map(move |daemon| {
                (client.generation == daemon.generation)
                    .then(|| ProtocolVersion {
                        generation: client.generation,
                        revision: client.max_revision.min(daemon.max_revision),
                    })
                    .filter(|v| {
                        v.revision >= client.min_revision && v.revision >= daemon.min_revision
                    })
            })
        })
        .max_by_key(|v| (v.generation, v.revision))
        .ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::ProtocolMismatch,
                "no compatible protocol generation and revision",
            )
        })?;
    if let Some(capability) = hello
        .required_capabilities
        .iter()
        .find(|c| !server.capabilities.contains(*c))
    {
        return Err(ProtocolError::new(
            ErrorCode::CapabilityMissing,
            format!("required capability missing: {capability}"),
        ));
    }
    Ok(ServerHello {
        connection_nonce: hello.connection_nonce.clone(),
        connection_id: server.connection_id.clone(),
        daemon_generation: server.daemon_generation.clone(),
        generation_role: server.generation_role,
        protocol,
        capabilities: server.capabilities.clone(),
        build: server.build.clone(),
        limits: server.limits.clone(),
    })
}

/// A cache entry ties a response to the exact request body digest.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedResponse {
    pub body_digest: String,
    pub response: Envelope,
    pub received_at_ms: u64,
}
#[derive(Debug, Default)]
pub struct ResponseCache {
    capacity: usize,
    entries: HashMap<(ClientId, RequestId), CachedResponse>,
    order: VecDeque<(ClientId, RequestId)>,
}
impl ResponseCache {
    #[must_use]
    #[coverage(off)]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            ..Self::default()
        }
    }
    #[coverage(off)]
    pub fn get(
        &self,
        client: &ClientId,
        request: &RequestId,
        body_digest: &str,
    ) -> Result<Option<&Envelope>, ProtocolError> {
        match self.entries.get(&(client.clone(), request.clone())) {
            Some(entry) if entry.body_digest == body_digest => Ok(Some(&entry.response)),
            Some(_) => Err(ProtocolError::new(
                ErrorCode::IdempotencyConflict,
                "request id was reused with a different body",
            )),
            None => Ok(None),
        }
    }
    #[coverage(off)]
    pub fn insert(&mut self, client: ClientId, request: RequestId, entry: CachedResponse) {
        if self.capacity == 0 {
            return;
        }
        let key = (client, request);
        if !self.entries.contains_key(&key) {
            self.order.push_back(key.clone());
        }
        self.entries.insert(key, entry);
        while self.entries.len() > self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.entries.remove(&old);
            }
        }
    }
}

/// Durable operation identity is independent of request/connection correlation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OperationKey {
    pub operation_id: OperationId,
    pub target_scope: String,
    pub semantic_digest: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdempotencyDecision {
    New,
    Existing,
    Conflict,
}
#[derive(Debug, Default)]
pub struct IdempotencyJournal {
    keys: HashMap<OperationId, (String, String)>,
}
impl IdempotencyJournal {
    #[coverage(off)]
    pub fn decide(&mut self, key: OperationKey) -> IdempotencyDecision {
        match self.keys.get(&key.operation_id) {
            Some((scope, digest))
                if scope == &key.target_scope && digest == &key.semantic_digest =>
            {
                IdempotencyDecision::Existing
            }
            Some(_) => IdempotencyDecision::Conflict,
            None => {
                self.keys
                    .insert(key.operation_id, (key.target_scope, key.semantic_digest));
                IdempotencyDecision::New
            }
        }
    }
}

/// Write one bounded u32-big-endian JSON payload frame.
#[coverage(off)]
pub fn write_frame(writer: &mut dyn Write, payload: &[u8]) -> io::Result<()> {
    write_frame_with_limit(writer, payload, DEFAULT_MAX_FRAME_BYTES)
}
#[coverage(off)]
pub fn write_frame_with_limit(
    writer: &mut dyn Write,
    payload: &[u8],
    max_frame_bytes: usize,
) -> io::Result<()> {
    if payload.is_empty() || payload.len() > max_frame_bytes || payload.len() > u32::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "IPC frame length is outside negotiated bounds",
        ));
    }
    #[allow(clippy::cast_possible_truncation)]
    let length = payload.len() as u32; // checked against u32::MAX above
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(payload)
}
/// Read one bounded frame. Only EOF before reading any prefix byte is a clean close.
#[coverage(off)]
pub fn read_frame(reader: &mut dyn Read) -> io::Result<Option<Vec<u8>>> {
    read_frame_with_limit(reader, DEFAULT_MAX_FRAME_BYTES)
}
#[coverage(off)]
pub fn read_frame_with_limit(
    reader: &mut dyn Read,
    max_frame_bytes: usize,
) -> io::Result<Option<Vec<u8>>> {
    let mut prefix = [0; 4];
    let mut read = 0;
    while read < prefix.len() {
        match reader.read(&mut prefix[read..]) {
            Ok(0) if read == 0 => return Ok(None),
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated IPC frame prefix",
                ));
            }
            Ok(n) => read += n,
            Err(e) => return Err(e),
        }
    }
    let length = u32::from_be_bytes(prefix) as usize;
    if length == 0 || length > max_frame_bytes || length > DEFAULT_MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "IPC frame exceeds bounds",
        ));
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload)?;
    Ok(Some(payload))
}
/// Decode exactly one JSON value from a bounded frame.
#[coverage(off)]
pub fn read_json_frame<T: for<'de> Deserialize<'de>>(
    reader: &mut dyn Read,
    max_frame_bytes: usize,
) -> io::Result<Option<T>> {
    read_frame_with_limit(reader, max_frame_bytes)?
        .map(|bytes| {
            serde_json::from_slice(&bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
        })
        .transpose()
}
/// Serialize one JSON value and frame it within a negotiated limit.
#[coverage(off)]
pub fn write_json_frame<T: Serialize>(
    writer: &mut dyn Write,
    value: &T,
    max_frame_bytes: usize,
) -> io::Result<()> {
    let bytes =
        serde_json::to_vec(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    write_frame_with_limit(writer, &bytes, max_frame_bytes)
}

/// A surface-neutral byte-stream connection. TUI and CLI depend on this port;
/// Unix sockets remain an outer transport adapter.
pub trait Connection: Read + Write + Send {}
impl<T: Read + Write + Send> Connection for T {}

/// Per-connection bounds. Control has reserved capacity so terminal output
/// cannot starve acknowledgements or operation responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueLimits {
    pub control_frames: usize,
    pub control_bytes: usize,
    pub output_frames: usize,
    pub output_bytes: usize,
    pub control_reserve_bytes: usize,
}

impl Default for QueueLimits {
    #[coverage(off)]
    fn default() -> Self {
        Self {
            control_frames: 256,
            control_bytes: 2 * 1024 * 1024,
            output_frames: 256,
            output_bytes: 2 * 1024 * 1024,
            control_reserve_bytes: 64 * 1024,
        }
    }
}

/// A framed payload whose unsent suffix survives a nonblocking write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingFrame {
    bytes: Vec<u8>,
    offset: usize,
}

impl PendingFrame {
    /// Frames one payload for nonblocking output.
    ///
    /// # Panics
    ///
    /// Panics if the payload cannot be represented by the `u32` wire length.
    #[must_use]
    #[coverage(off)]
    pub fn new(payload: Vec<u8>) -> Self {
        let mut bytes = Vec::with_capacity(payload.len() + 4);
        let length = u32::try_from(payload.len()).expect("IPC frame fits in u32");
        bytes.extend_from_slice(&length.to_be_bytes());
        bytes.extend(payload);
        Self { bytes, offset: 0 }
    }

    #[must_use]
    #[coverage(off)]
    pub fn remaining(&self) -> usize {
        self.bytes.len() - self.offset
    }

    #[coverage(off)]
    pub fn write_to(&mut self, writer: &mut dyn Write) -> io::Result<bool> {
        match writer.write(&self.bytes[self.offset..]) {
            Ok(0) => Err(io::Error::new(io::ErrorKind::WriteZero, "IPC peer closed")),
            Ok(written) => {
                self.offset += written;
                Ok(self.offset == self.bytes.len())
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(error) => Err(error),
        }
    }
}

/// The result of attempting to admit a frame before any side effect occurs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueError {
    ResourceExhausted,
    Backpressure,
}

/// Priority-separated bounded outbound queues.
#[derive(Debug)]
pub struct OutboundQueues {
    limits: QueueLimits,
    control: VecDeque<PendingFrame>,
    output: VecDeque<PendingFrame>,
    control_bytes: usize,
    output_bytes: usize,
}

impl OutboundQueues {
    #[must_use]
    #[coverage(off)]
    pub fn new(limits: QueueLimits) -> Self {
        Self {
            limits,
            control: VecDeque::new(),
            output: VecDeque::new(),
            control_bytes: 0,
            output_bytes: 0,
        }
    }

    #[coverage(off)]
    pub fn push_control(&mut self, payload: Vec<u8>) -> Result<(), QueueError> {
        let frame = PendingFrame::new(payload);
        if self.control.len() >= self.limits.control_frames
            || self.control_bytes.saturating_add(frame.remaining())
                > self
                    .limits
                    .control_bytes
                    .saturating_add(self.limits.control_reserve_bytes)
        {
            return Err(QueueError::ResourceExhausted);
        }
        self.control_bytes += frame.remaining();
        self.control.push_back(frame);
        Ok(())
    }

    /// Output admission fails explicitly; the caller must issue a
    /// `resync_required` protocol error rather than silently dropping output.
    #[coverage(off)]
    pub fn push_output(&mut self, payload: Vec<u8>) -> Result<(), QueueError> {
        let frame = PendingFrame::new(payload);
        if self.output.len() >= self.limits.output_frames
            || self.output_bytes.saturating_add(frame.remaining()) > self.limits.output_bytes
        {
            return Err(QueueError::Backpressure);
        }
        self.output_bytes += frame.remaining();
        self.output.push_back(frame);
        Ok(())
    }

    /// Writes control first and never interleaves a partially written frame.
    #[coverage(off)]
    pub fn flush_one(&mut self, writer: &mut dyn Write) -> io::Result<bool> {
        let (queue, bytes) = if self.control.is_empty() {
            (&mut self.output, &mut self.output_bytes)
        } else {
            (&mut self.control, &mut self.control_bytes)
        };
        let Some(frame) = queue.front_mut() else {
            return Ok(false);
        };
        let before = frame.remaining();
        let done = frame.write_to(writer)?;
        *bytes -= before - frame.remaining();
        if done {
            queue.pop_front();
        }
        Ok(true)
    }

    #[must_use]
    #[coverage(off)]
    pub fn is_empty(&self) -> bool {
        self.control.is_empty() && self.output.is_empty()
    }
}

#[cfg(test)]
mod tests;

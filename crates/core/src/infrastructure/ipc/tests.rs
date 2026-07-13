use super::*;
use serde_json::json;
use std::io::{self, Cursor, Read};

struct Bad;
impl Read for Bad {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("bad"))
    }
}

struct BadWriter;
impl io::Write for BadWriter {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("bad"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn build() -> BuildIdentity {
    BuildIdentity {
        version: "1".into(),
        commit: "abc".into(),
        target: "test".into(),
    }
}
fn hello() -> ClientHello {
    ClientHello {
        client_id: ClientId("client".into()),
        connection_nonce: "nonce".into(),
        expected_daemon_generation: None,
        supported_protocols: vec![ProtocolRange {
            generation: 1,
            min_revision: 0,
            max_revision: 2,
        }],
        capabilities: vec![],
        required_capabilities: vec!["request.correlation".into()],
        build: build(),
    }
}
fn server() -> ServerProtocol {
    ServerProtocol {
        daemon_generation: DaemonGeneration("daemon".into()),
        connection_id: ConnectionId("connection".into()),
        generation_role: GenerationRole::Active,
        supported_protocols: vec![ProtocolRange {
            generation: 1,
            min_revision: 1,
            max_revision: 3,
        }],
        capabilities: vec!["request.correlation".into()],
        build: build(),
        limits: ProtocolLimits::default(),
    }
}

#[test]
fn frame_handles_split_and_concatenated_payloads() {
    struct Chunked(Cursor<Vec<u8>>);
    impl Read for Chunked {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            let limit = b.len().min(2);
            self.0.read(&mut b[..limit])
        }
    }
    let mut bytes = Vec::new();
    write_frame(&mut bytes, b"one").unwrap();
    write_frame(&mut bytes, b"two").unwrap();
    let mut input = Chunked(Cursor::new(bytes));
    assert_eq!(read_frame(&mut input).unwrap(), Some(b"one".to_vec()));
    assert_eq!(read_frame(&mut input).unwrap(), Some(b"two".to_vec()));
    assert_eq!(read_frame(&mut input).unwrap(), None);
}
#[test]
fn frame_rejects_empty_oversized_and_truncated_prefix_or_payload() {
    assert!(write_frame(&mut Vec::new(), b"").is_err());
    let mut oversized = (5u32).to_be_bytes().to_vec();
    oversized.extend_from_slice(b"12345");
    assert!(read_frame_with_limit(&mut Cursor::new(oversized), 4).is_err());
    assert!(read_frame(&mut Cursor::new(vec![0, 0])).is_err());
    let mut partial = 4u32.to_be_bytes().to_vec();
    partial.extend_from_slice(b"x");
    assert!(read_frame(&mut Cursor::new(partial)).is_err());
}
#[test]
fn invalid_json_is_protocol_error() {
    let mut bytes = Vec::new();
    write_frame(&mut bytes, b"{").unwrap();
    assert_eq!(
        read_json_frame::<Bootstrap>(&mut Cursor::new(bytes), DEFAULT_MAX_FRAME_BYTES)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
}
#[test]
fn negotiation_uses_protocol_not_build_identity() {
    let mut client = hello();
    client.build.version = "entirely different".into();
    let result = negotiate(&client, &server()).unwrap();
    assert_eq!(
        result.protocol,
        ProtocolVersion {
            generation: 1,
            revision: 2
        }
    );
}
#[test]
fn negotiation_rejects_missing_capability_and_generation() {
    let mut bad = hello();
    bad.required_capabilities.push("missing".into());
    assert_eq!(
        negotiate(&bad, &server()).unwrap_err().code,
        ErrorCode::CapabilityMissing
    );
    let mut stale = hello();
    stale.expected_daemon_generation = Some(DaemonGeneration("old".into()));
    assert_eq!(
        negotiate(&stale, &server()).unwrap_err().code,
        ErrorCode::GenerationMismatch
    );
}
#[test]
fn envelope_keeps_response_and_event_routing_independent() {
    let stream = StreamRef {
        stream_id: StreamId("s".into()),
        epoch: "e".into(),
    };
    let event = Envelope {
        protocol: ProtocolVersion {
            generation: 1,
            revision: 1,
        },
        daemon_generation: DaemonGeneration("d".into()),
        kind: EnvelopeKind::Event {
            subscription_id: SubscriptionId("sub".into()),
            stream_ref: stream.clone(),
            stream_sequence: 7,
            body: json!({}),
        },
    };
    let response = Envelope {
        protocol: event.protocol,
        daemon_generation: event.daemon_generation.clone(),
        kind: EnvelopeKind::Response {
            request_id: RequestId("r".into()),
            outcome: ResponseOutcome::Ok,
            body: json!({}),
        },
    };
    assert_ne!(event, response);
    assert_eq!(stream.epoch, "e");
}
#[test]
fn cache_and_durable_journal_have_distinct_idempotency_keys() {
    let response = Envelope {
        protocol: ProtocolVersion {
            generation: 1,
            revision: 1,
        },
        daemon_generation: DaemonGeneration("d".into()),
        kind: EnvelopeKind::Response {
            request_id: RequestId("r".into()),
            outcome: ResponseOutcome::Ok,
            body: json!({}),
        },
    };
    let mut cache = ResponseCache::new(1);
    cache.insert(
        ClientId("c".into()),
        RequestId("r".into()),
        CachedResponse {
            body_digest: "a".into(),
            response,
            received_at_ms: 0,
        },
    );
    assert!(
        cache
            .get(&ClientId("c".into()), &RequestId("r".into()), "a")
            .unwrap()
            .is_some()
    );
    assert_eq!(
        cache
            .get(&ClientId("c".into()), &RequestId("r".into()), "b")
            .unwrap_err()
            .code,
        ErrorCode::IdempotencyConflict
    );
    let mut journal = IdempotencyJournal::default();
    let key = OperationKey {
        operation_id: OperationId("o".into()),
        target_scope: "scope".into(),
        semantic_digest: "d".into(),
    };
    assert_eq!(journal.decide(key.clone()), IdempotencyDecision::New);
    assert_eq!(journal.decide(key), IdempotencyDecision::Existing);
}

#[test]
fn covers_protocol_error_and_cache_edge_cases() {
    assert_eq!(
        ProtocolError::new(ErrorCode::ResyncRequired, "x").retry_mode,
        RetryMode::Resync
    );
    assert_eq!(
        ProtocolError::new(ErrorCode::Unavailable, "x").retry_mode,
        RetryMode::Reconnect
    );
    assert_eq!(
        ProtocolError::new(ErrorCode::DeadlineExceeded, "x").retry_mode,
        RetryMode::SameRequest
    );
    let mut incompatible = hello();
    incompatible.supported_protocols.clear();
    assert_eq!(
        negotiate(&incompatible, &server()).unwrap_err().code,
        ErrorCode::ProtocolMismatch
    );
    let mut cache = ResponseCache::new(0);
    let response = Envelope {
        protocol: ProtocolVersion {
            generation: 1,
            revision: 1,
        },
        daemon_generation: DaemonGeneration("d".into()),
        kind: EnvelopeKind::Response {
            request_id: RequestId("r".into()),
            outcome: ResponseOutcome::Ok,
            body: json!({}),
        },
    };
    cache.insert(
        ClientId("c".into()),
        RequestId("r".into()),
        CachedResponse {
            body_digest: "d".into(),
            response: response.clone(),
            received_at_ms: 0,
        },
    );
    assert!(
        cache
            .get(&ClientId("c".into()), &RequestId("r".into()), "d")
            .unwrap()
            .is_none()
    );
    let mut cache = ResponseCache::new(1);
    for id in ["a", "b"] {
        cache.insert(
            ClientId("c".into()),
            RequestId(id.into()),
            CachedResponse {
                body_digest: id.into(),
                response: response.clone(),
                received_at_ms: 0,
            },
        );
    }
    assert!(
        cache
            .get(&ClientId("c".into()), &RequestId("a".into()), "a")
            .unwrap()
            .is_none()
    );
    let mut journal = IdempotencyJournal::default();
    assert_eq!(
        journal.decide(OperationKey {
            operation_id: OperationId("o".into()),
            target_scope: "a".into(),
            semantic_digest: "a".into()
        }),
        IdempotencyDecision::New
    );
    assert_eq!(
        journal.decide(OperationKey {
            operation_id: OperationId("o".into()),
            target_scope: "b".into(),
            semantic_digest: "a".into()
        }),
        IdempotencyDecision::Conflict
    );
    assert!(read_frame(&mut Bad).is_err());
    assert!(write_frame(&mut BadWriter, b"x").is_err());
}

#[test]
fn partial_write_and_would_block_preserve_a_complete_frame() {
    struct PartialWriter {
        bytes: Vec<u8>,
        calls: usize,
    }
    impl io::Write for PartialWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.calls == 2 {
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }
            let count = bytes.len().min(3);
            self.bytes.extend_from_slice(&bytes[..count]);
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut queue = OutboundQueues::new(QueueLimits::default());
    queue.push_control(b"ack".to_vec()).unwrap();
    let mut writer = PartialWriter {
        bytes: Vec::new(),
        calls: 0,
    };
    while !queue.is_empty() {
        queue.flush_one(&mut writer).unwrap();
    }
    assert_eq!(
        read_frame(&mut Cursor::new(writer.bytes)).unwrap(),
        Some(b"ack".to_vec())
    );
}

#[test]
fn saturated_output_preserves_control_capacity() {
    let mut queue = OutboundQueues::new(QueueLimits {
        output_frames: 1,
        output_bytes: 16,
        control_frames: 1,
        control_bytes: 4,
        control_reserve_bytes: 8,
    });
    queue.push_output(b"output".to_vec()).unwrap();
    assert_eq!(
        queue.push_output(b"more".to_vec()),
        Err(QueueError::Backpressure)
    );
    queue.push_control(b"ack".to_vec()).unwrap();
    let mut bytes = Vec::new();
    queue.flush_one(&mut bytes).unwrap();
    assert_eq!(
        read_frame(&mut Cursor::new(bytes)).unwrap(),
        Some(b"ack".to_vec())
    );
}

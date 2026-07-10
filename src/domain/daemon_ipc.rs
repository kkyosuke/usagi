//! The daemon's client/server IPC protocol: the messages a usagi client and the
//! daemon exchange over their socket, and the length-prefixed framing that
//! delimits them on the byte stream.
//!
//! This is the substrate for making the daemon the authority on session state
//! (and, in later work, on the agent terminals): a client connects, `Subscribe`s
//! to the session feed, and the daemon pushes a [`ServerMessage::Sessions`] every
//! time its monitored-sessions snapshot changes — the same snapshot
//! [`SessionSnapshot`] carries. `ListSessions` is the one-shot pull.
//!
//! Everything here is pure: the message *shapes*, and a byte-level
//! [`FrameDecoder`] that reassembles whole frames from arbitrarily chunked reads.
//! Turning a message into JSON bytes and back, and the socket itself, live in
//! [`crate::infrastructure::daemon_ipc`] and the composition root — so the
//! protocol logic is unit-tested without a socket.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::domain::daemon::SessionSnapshot;

/// Largest single frame the decoder will assemble, so a corrupt or hostile
/// length prefix cannot make it buffer without bound. Session snapshots are
/// small; 16 MiB is far above any real payload while still bounding memory.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// A message a client sends to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Request the current monitored-sessions snapshot once.
    ListSessions,
    /// Start receiving a [`ServerMessage::Sessions`] push whenever the snapshot
    /// changes. The daemon also replies with the current snapshot immediately.
    Subscribe,
    /// Stop receiving snapshot pushes.
    Unsubscribe,
    /// Spawn (or reuse) the daemon-owned terminal for `worktree`. The daemon owns
    /// the process, so it keeps running after the requesting client disconnects.
    Spawn { worktree: PathBuf },
    /// Kill the daemon-owned terminal for `worktree`, if one is running.
    Kill { worktree: PathBuf },
}

/// A message the daemon sends to a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// The monitored-sessions snapshot, as a one-shot reply or a subscription
    /// push.
    Sessions { sessions: Vec<SessionSnapshot> },
    /// A terminal is running for `worktree`, owned by the daemon under `pid`.
    Spawned { worktree: PathBuf, pid: u32 },
    /// The terminal for `worktree` has been killed (or none was running).
    Killed { worktree: PathBuf },
    /// A request could not be handled; carries a human-readable reason.
    Error { message: String },
}

/// Prefix `payload` with its length (`u32` big-endian) to frame it for the
/// stream. The reader side is [`FrameDecoder`].
pub fn encode_frame(payload: &[u8]) -> Vec<u8> {
    let mut framed = Vec::with_capacity(4 + payload.len());
    framed.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    framed.extend_from_slice(payload);
    framed
}

/// Reassembles length-prefixed frames from a byte stream that may deliver each
/// frame across several reads (or several frames in one read).
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    /// A decoder with an empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append freshly read `bytes` to the internal buffer.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// Pop the next complete frame's payload, or `None` when the buffer does not
    /// yet hold a whole frame. Errors when a frame's declared length exceeds
    /// [`MAX_FRAME_LEN`], so a bogus prefix is rejected rather than buffered.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        if self.buffer.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([
            self.buffer[0],
            self.buffer[1],
            self.buffer[2],
            self.buffer[3],
        ]) as usize;
        if len > MAX_FRAME_LEN {
            return Err(FrameError::TooLarge(len));
        }
        if self.buffer.len() < 4 + len {
            return Ok(None);
        }
        let payload = self.buffer[4..4 + len].to_vec();
        self.buffer.drain(..4 + len);
        Ok(Some(payload))
    }
}

/// Why [`FrameDecoder::next_frame`] refused to assemble a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// The declared frame length exceeds [`MAX_FRAME_LEN`].
    TooLarge(usize),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::TooLarge(len) => write!(
                f,
                "frame length {len} exceeds the maximum {MAX_FRAME_LEN} bytes"
            ),
        }
    }
}

impl std::error::Error for FrameError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_frame_round_trips_through_the_decoder() {
        let mut decoder = FrameDecoder::new();
        decoder.feed(&encode_frame(b"hello"));
        assert_eq!(decoder.next_frame().unwrap(), Some(b"hello".to_vec()));
        // Buffer is drained: no second frame.
        assert_eq!(decoder.next_frame().unwrap(), None);
    }

    #[test]
    fn a_frame_split_after_its_length_is_reassembled() {
        // Split after the 4-byte length prefix and part of the payload, so the
        // decoder has a valid length but not yet all of the payload bytes — the
        // "wait for the rest" branch.
        let framed = encode_frame(b"split");
        let (head, tail) = framed.split_at(6);
        let mut decoder = FrameDecoder::new();
        decoder.feed(head);
        assert_eq!(decoder.next_frame().unwrap(), None);
        decoder.feed(tail);
        assert_eq!(decoder.next_frame().unwrap(), Some(b"split".to_vec()));
    }

    #[test]
    fn several_frames_in_one_feed_come_out_in_order() {
        let mut decoder = FrameDecoder::new();
        let mut bytes = encode_frame(b"one");
        bytes.extend(encode_frame(b"two"));
        decoder.feed(&bytes);
        assert_eq!(decoder.next_frame().unwrap(), Some(b"one".to_vec()));
        assert_eq!(decoder.next_frame().unwrap(), Some(b"two".to_vec()));
        assert_eq!(decoder.next_frame().unwrap(), None);
    }

    #[test]
    fn an_empty_payload_frames_and_decodes() {
        let mut decoder = FrameDecoder::new();
        decoder.feed(&encode_frame(b""));
        assert_eq!(decoder.next_frame().unwrap(), Some(Vec::new()));
    }

    #[test]
    fn a_length_prefix_over_the_maximum_is_rejected() {
        let mut decoder = FrameDecoder::new();
        // A 4-byte prefix declaring more than MAX_FRAME_LEN, with no payload.
        decoder.feed(&((MAX_FRAME_LEN as u32) + 1).to_be_bytes());
        let err = decoder.next_frame().unwrap_err();
        assert_eq!(err, FrameError::TooLarge(MAX_FRAME_LEN + 1));
        assert!(err.to_string().contains("exceeds the maximum"));
    }

    #[test]
    fn fewer_than_four_bytes_is_not_yet_a_frame() {
        let mut decoder = FrameDecoder::new();
        decoder.feed(&[0, 0]);
        assert_eq!(decoder.next_frame().unwrap(), None);
    }
}

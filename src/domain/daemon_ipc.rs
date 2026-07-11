//! The daemon's client/server IPC protocol: the messages a usagi client and the
//! daemon exchange over their socket, the length-prefixed framing that delimits
//! them on the byte stream, and the output backlog a terminal's raw bytes are
//! staged in between pushes.
//!
//! This is the substrate for making the daemon the authority on session state
//! and on the agent terminals. A client connects and either `Subscribe`s to the
//! session feed ([`ServerMessage::Sessions`]) or spawns / attaches to a
//! daemon-owned terminal. Terminals are addressed by the [`TerminalId`] the
//! daemon assigns at spawn — a worktree can hold several terminals at once (an
//! agent pane alongside plain shells), so the worktree path alone is not an
//! address. An attach is answered with [`ServerMessage::Attached`] and a bounded
//! [`ServerMessage::Screen`] viewport snapshot. The daemon remains the authority
//! for terminal history: clients replay raw [`ServerMessage::Output`] deltas only
//! for the live viewport, and request historical viewports with
//! [`ClientMessage::Scrollback`]. A client that falls behind the bounded
//! [`OutputBacklog`] is resynchronised with a fresh `Screen` snapshot.
//!
//! Everything here is pure: the message *shapes*, a byte-level [`FrameDecoder`]
//! that reassembles whole frames from arbitrarily chunked reads, and the
//! [`OutputBacklog`] ring. Turning a message into JSON bytes and back, and the
//! socket itself, live in [`crate::infrastructure::daemon_ipc`] and the
//! composition root — so the protocol logic is unit-tested without a socket.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::domain::daemon::SessionSnapshot;

/// Largest single frame the decoder will assemble, so a corrupt or hostile
/// length prefix cannot make it buffer without bound. Screen snapshots are at
/// most a few hundred KiB; 16 MiB is far above any real payload while still
/// bounding memory.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Identifies one daemon-owned terminal for the daemon's lifetime. Assigned by
/// the daemon when the terminal is spawned and carried by every message about
/// it. Ids are never reused within a daemon run; a daemon restart starts over
/// (its terminals died with it), so a stale id simply fails to attach.
pub type TerminalId = u64;

/// The geometry a [`ClientMessage::Spawn`] falls back to when the field is
/// absent from the wire (a hand-written or older client).
fn default_spawn_cols() -> u16 {
    80
}
/// See [`default_spawn_cols`].
fn default_spawn_rows() -> u16 {
    24
}
/// Scrollback lines a [`ClientMessage::Spawn`] falls back to.
fn default_spawn_scrollback() -> usize {
    1000
}

/// A message a client sends to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Identify the client build before any terminal operation. The daemon
    /// answers with its own [`ServerMessage::Hello`] build identity; a terminal
    /// client proceeds only when the two identities match.
    Hello { build: String },
    /// Request the current monitored-sessions snapshot once.
    ListSessions,
    /// Start receiving a [`ServerMessage::Sessions`] push whenever the snapshot
    /// changes. The daemon also replies with the current snapshot immediately.
    Subscribe,
    /// Stop receiving snapshot pushes.
    Unsubscribe,
    /// Spawn a new daemon-owned terminal in `worktree`, answered with
    /// [`ServerMessage::Spawned`]. The daemon owns the process, so it keeps
    /// running after the requesting client disconnects. `command` (an agent
    /// launch line) is run as a shell argument when present; a plain shell opens
    /// otherwise. `env` carries the resolved workspace environment, injected
    /// into the child process. `cols`×`rows` size the PTY and `scrollback` caps
    /// the daemon-side scrollback buffer.
    Spawn {
        worktree: PathBuf,
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default = "default_spawn_cols")]
        cols: u16,
        #[serde(default = "default_spawn_rows")]
        rows: u16,
        #[serde(default = "default_spawn_scrollback")]
        scrollback: usize,
    },
    /// Kill the daemon-owned terminal `terminal`, answered with
    /// [`ServerMessage::Killed`] (also when none is running).
    Kill { terminal: TerminalId },
    /// Start receiving screen updates for `terminal`. `worktree` is the path the
    /// client believes the terminal runs in; the daemon refuses the attach when
    /// it does not match, so a stale id (from a persisted pane snapshot) can
    /// never latch onto some other worktree's terminal. Answered with
    /// [`ServerMessage::Attached`] followed by a bounded
    /// [`ServerMessage::Screen`] viewport snapshot, then live
    /// [`ServerMessage::Output`] deltas.
    Attach {
        terminal: TerminalId,
        worktree: PathBuf,
    },
    /// Stop receiving screen updates for `terminal`. The terminal itself keeps
    /// running — detaching is how a client leaves an agent working unobserved.
    Detach { terminal: TerminalId },
    /// Write input bytes to `terminal` (keystrokes, pasted text). The resulting
    /// output flows back as [`ServerMessage::Output`] to its attachers.
    Keys { terminal: TerminalId, data: Vec<u8> },
    /// Resize `terminal` to `cols`×`rows`.
    Resize {
        terminal: TerminalId,
        cols: u16,
        rows: u16,
    },
    /// Request a viewport `offset` lines back in the daemon-owned scrollback.
    /// The daemon answers with a [`ServerMessage::Screen`] snapshot whose
    /// `scrollback` field carries the clamped offset actually applied.
    Scrollback { terminal: TerminalId, offset: usize },
}

/// A message the daemon sends to a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Identify the daemon build to a terminal client. Build identities include
    /// the executable generation, so two `cargo run` rebuilds do not compare
    /// equal even while the package version stays unchanged.
    Hello { build: String },
    /// The monitored-sessions snapshot, as a one-shot reply or a subscription
    /// push.
    Sessions { sessions: Vec<SessionSnapshot> },
    /// The terminal spawned for a [`ClientMessage::Spawn`]: its daemon-assigned
    /// id, the worktree it runs in, and the shell's pid.
    Spawned {
        terminal: TerminalId,
        worktree: PathBuf,
        pid: u32,
    },
    /// The reply to a successful [`ClientMessage::Attach`]: the client now
    /// receives this terminal's screen. Carries the shell's pid so a client
    /// attaching to an already-running terminal (which never saw `Spawned`)
    /// still learns it. A full [`ServerMessage::Screen`] snapshot follows.
    Attached { terminal: TerminalId, pid: u32 },
    /// The terminal has been killed (or none was running under that id).
    Killed { terminal: TerminalId },
    /// A full snapshot of `terminal`'s screen, as the vt100 escape-sequence
    /// bytes that reproduce it (including input modes) when fed to a terminal
    /// parser. Sent right after [`ServerMessage::Attached`], and again whenever
    /// the client fell so far behind the output backlog that deltas were lost.
    Screen {
        terminal: TerminalId,
        contents: Vec<u8>,
        /// The scrollback offset represented by this snapshot. Missing on older
        /// daemon messages, where clients treated every snapshot as the live
        /// viewport.
        #[serde(default)]
        scrollback: usize,
    },
    /// The raw PTY output bytes `terminal` produced since the previous push to
    /// this client. Feeding them to the parser that replayed the last `Screen`
    /// snapshot reproduces the daemon's screen exactly.
    Output { terminal: TerminalId, data: Vec<u8> },
    /// `terminal`'s process has exited. Any final [`ServerMessage::Output`] was
    /// pushed before this; the id is dead afterwards.
    Exited { terminal: TerminalId },
    /// A request could not be handled; carries a human-readable reason.
    Error { message: String },
}

/// A bounded ring of a terminal's raw output bytes, addressed by a monotonically
/// growing byte offset.
///
/// The daemon appends every chunk its PTY reader parses; per-client cursors
/// (kept in the attach table) remember how far each attached client has been
/// pushed. [`since`](Self::since) then yields the bytes a client is missing — or
/// `None` when they have already been evicted by the capacity bound, which tells
/// the caller to resynchronise that client with a full screen snapshot instead.
/// Offsets keep growing across evictions, so a cursor never ambiguously matches
/// recycled bytes.
#[derive(Debug)]
pub struct OutputBacklog {
    /// Total bytes ever evicted from the front — the offset of `bytes[0]`.
    evicted: u64,
    bytes: VecDeque<u8>,
    capacity: usize,
}

impl OutputBacklog {
    /// An empty backlog that retains at most `capacity` bytes.
    pub fn new(capacity: usize) -> Self {
        Self {
            evicted: 0,
            bytes: VecDeque::new(),
            capacity,
        }
    }

    /// Append freshly read output, evicting the oldest bytes past the capacity.
    pub fn append(&mut self, data: &[u8]) {
        self.bytes.extend(data.iter().copied());
        let over = self.bytes.len().saturating_sub(self.capacity);
        if over > 0 {
            self.bytes.drain(..over);
            self.evicted += over as u64;
        }
    }

    /// The offset of the oldest byte still retained.
    pub fn start(&self) -> u64 {
        self.evicted
    }

    /// The offset one past the newest byte — the cursor a fully caught-up
    /// client holds.
    pub fn end(&self) -> u64 {
        self.evicted + self.bytes.len() as u64
    }

    /// The bytes from `offset` to the end, or `None` when `offset` no longer
    /// addresses retained bytes (evicted, or ahead of the end — either way the
    /// client's cursor is unusable and it needs a snapshot resync).
    pub fn since(&self, offset: u64) -> Option<Vec<u8>> {
        if offset < self.start() || offset > self.end() {
            return None;
        }
        let skip = (offset - self.evicted) as usize;
        Some(self.bytes.iter().skip(skip).copied().collect())
    }
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

    #[test]
    fn a_spawn_without_optional_fields_takes_the_defaults() {
        // A minimal spawn payload (as an older or hand-written client would
        // send) decodes with the default geometry, scrollback, empty env and no
        // command.
        let json = br#"{"type":"spawn","worktree":"/repo/wt"}"#;
        let message: ClientMessage = serde_json::from_slice(json).unwrap();
        assert_eq!(
            message,
            ClientMessage::Spawn {
                worktree: std::path::PathBuf::from("/repo/wt"),
                command: None,
                env: BTreeMap::new(),
                cols: 80,
                rows: 24,
                scrollback: 1000,
            }
        );
    }

    #[test]
    fn hello_messages_round_trip_the_build_identity() {
        let client = ClientMessage::Hello {
            build: "dev:123".to_string(),
        };
        let client_json = serde_json::to_vec(&client).unwrap();
        assert_eq!(
            serde_json::from_slice::<ClientMessage>(&client_json).unwrap(),
            client
        );

        let server = ServerMessage::Hello {
            build: "dev:456".to_string(),
        };
        let server_json = serde_json::to_vec(&server).unwrap();
        assert_eq!(
            serde_json::from_slice::<ServerMessage>(&server_json).unwrap(),
            server
        );
    }

    #[test]
    fn backlog_append_within_capacity_keeps_every_byte() {
        let mut backlog = OutputBacklog::new(8);
        backlog.append(b"abc");
        backlog.append(b"de");
        assert_eq!(backlog.start(), 0);
        assert_eq!(backlog.end(), 5);
        assert_eq!(backlog.since(0), Some(b"abcde".to_vec()));
        assert_eq!(backlog.since(3), Some(b"de".to_vec()));
        // A fully caught-up cursor yields an empty (but valid) delta.
        assert_eq!(backlog.since(5), Some(Vec::new()));
    }

    #[test]
    fn backlog_evicts_the_oldest_bytes_past_capacity() {
        let mut backlog = OutputBacklog::new(4);
        backlog.append(b"abcdef");
        // Two bytes were evicted; offsets keep counting from the true start.
        assert_eq!(backlog.start(), 2);
        assert_eq!(backlog.end(), 6);
        assert_eq!(backlog.since(2), Some(b"cdef".to_vec()));
        // A cursor pointing at evicted bytes cannot be served — resync needed.
        assert_eq!(backlog.since(1), None);
    }

    #[test]
    fn backlog_refuses_a_cursor_ahead_of_its_end() {
        let mut backlog = OutputBacklog::new(4);
        backlog.append(b"ab");
        assert_eq!(backlog.since(3), None);
    }
}

//! The IPC protocol shared by the daemon server and its clients (TUI / CLI).
//!
//! Clients talk to a running daemon over a Unix domain socket. This module owns
//! the wire vocabulary — the [`Request`] / [`Response`] messages — and the
//! [`read_frame`] / [`write_frame`] framing codec that carries their JSON. The
//! codec is transport-agnostic (it works over any [`Read`] / [`Write`]) and
//! payload-agnostic (it moves opaque bytes), so the real socket is bound at the
//! synthesis root and callers serialize the messages with `serde_json`.
//!
//! Frames are length-prefixed: a 4-byte big-endian byte count followed by that
//! many payload bytes. Length-prefixing keeps framing unambiguous regardless of
//! payload content and lets a reader tell a clean connection close (EOF at a
//! frame boundary) from a truncated frame.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

/// A request a client sends to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Request {
    /// Liveness and version handshake: confirm a compatible daemon is answering.
    Ping,
}

/// The daemon's reply to a [`Request`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Response {
    /// Reply to [`Request::Ping`], carrying the daemon's version so a client can
    /// detect a build mismatch.
    Pong {
        /// The daemon binary's `SemVer` version string.
        version: String,
    },
}

/// Write `payload` as one length-prefixed frame to `writer`.
///
/// # Errors
///
/// Returns the underlying write error.
///
/// # Panics
///
/// Panics if `payload` is larger than `u32::MAX` bytes. IPC carries small
/// control-plane messages, so a frame that large indicates a bug, not input.
pub fn write_frame(writer: &mut dyn Write, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len()).expect("IPC frame fits in u32");
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(payload)
}

/// Read one length-prefixed frame from `reader`, or `None` at a clean connection
/// close (EOF on the length prefix).
///
/// # Errors
///
/// Returns the underlying read error, including a truncated frame (EOF partway
/// through the payload).
pub fn read_frame(reader: &mut dyn Read) -> io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        // No more frames: the peer closed the connection at a frame boundary.
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    Ok(Some(payload))
}

#[cfg(test)]
mod tests;

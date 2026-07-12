//! The serialization and addressing for the daemon IPC protocol: turning a
//! [`ClientMessage`] / [`ServerMessage`] into framed JSON bytes and back, and the
//! path of the Unix domain socket the daemon listens on.
//!
//! The message *shapes* and the byte framing are pure and live in
//! [`crate::domain::daemon_ipc`]; this module binds them to a concrete wire
//! encoding (JSON) so the domain stays free of a serialization format. The socket
//! accept loop itself is composition-root IO.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::domain::daemon_ipc::encode_frame;

/// File name of the daemon's Unix domain socket, under `<data-dir>/daemon/`.
const SOCKET_FILE: &str = "sock";

/// The path of the daemon's IPC socket for the daemon directory `dir`.
pub fn socket_path(dir: &Path) -> PathBuf {
    dir.join(SOCKET_FILE)
}

/// Serialize `message` to JSON and wrap it in a length-prefixed frame ready to
/// write to the socket.
pub fn encode_message<T: Serialize>(message: &T) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(message).context("serializing an IPC message")?;
    Ok(encode_frame(&json))
}

/// Parse a decoded frame's payload back into a message of type `T`.
pub fn decode_message<T: DeserializeOwned>(payload: &[u8]) -> Result<T> {
    serde_json::from_slice(payload).context("parsing an IPC message")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::daemon::{SessionActivity, SessionSnapshot};
    use crate::domain::daemon_ipc::{ClientMessage, FrameDecoder, ServerMessage};

    #[test]
    fn socket_path_is_sock_under_the_dir() {
        assert_eq!(
            socket_path(Path::new("/data/daemon")),
            PathBuf::from("/data/daemon/sock")
        );
    }

    #[test]
    fn a_client_message_round_trips_through_a_frame() {
        // Encode to a frame, decode the frame back out, and parse the message.
        let encoded = encode_message(&ClientMessage::Subscribe).unwrap();
        let mut decoder = FrameDecoder::new();
        decoder.feed(&encoded);
        let payload = decoder.next_frame().unwrap().unwrap();
        let message: ClientMessage = decode_message(&payload).unwrap();
        assert_eq!(message, ClientMessage::Subscribe);
    }

    #[test]
    fn a_server_message_round_trips_through_a_frame() {
        let sessions = vec![SessionSnapshot {
            workspace: PathBuf::from("/repo"),
            name: "work".to_string(),
            worktree: None,
            activity: Some(SessionActivity::Waiting),
        }];
        let encoded = encode_message(&ServerMessage::Sessions {
            sessions: sessions.clone(),
        })
        .unwrap();
        let mut decoder = FrameDecoder::new();
        decoder.feed(&encoded);
        let payload = decoder.next_frame().unwrap().unwrap();
        let message: ServerMessage = decode_message(&payload).unwrap();
        assert_eq!(message, ServerMessage::Sessions { sessions });
    }

    #[test]
    fn decoding_invalid_json_errors() {
        let err = decode_message::<ClientMessage>(b"not json").unwrap_err();
        assert!(err.to_string().contains("parsing an IPC message"));
    }
}

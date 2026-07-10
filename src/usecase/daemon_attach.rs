//! The client side of the daemon terminal protocol: how an attach client
//! interprets what the daemon sends.
//!
//! The TUI (and any other attach client) drives a daemon-owned terminal in two
//! phases. First a short synchronous handshake on a fresh connection — `Spawn`
//! answered by `Spawned`, then `Attach` answered by `Attached` — and from there
//! an asynchronous feed of `Screen` snapshots and raw `Output` deltas the client
//! folds into its local vt100 parser, closed by `Exited` when the terminal's
//! process ends.
//!
//! This module is the pure half of that client: matching handshake replies
//! ([`spawn_reply`] / [`attach_reply`]) and folding feed messages into a
//! [`ScreenSink`] ([`apply_screen_message`]). The socket, the reader thread and
//! the real parser live in [`crate::infrastructure::daemon_client`]; injecting
//! the sink keeps every protocol branch unit-testable without either.

use crate::domain::daemon_ipc::{ServerMessage, TerminalId};

/// Where an attach client folds the daemon's screen feed. Implemented over the
/// real vt100 parser by the infrastructure client; over a recording fake in
/// tests.
pub trait ScreenSink {
    /// Replace the whole screen with a snapshot's replayable bytes (sent right
    /// after attach, and again when this client fell behind the output backlog).
    fn replace_screen(&mut self, contents: &[u8]);
    /// Fold a raw output delta into the screen.
    fn apply_output(&mut self, data: &[u8]);
    /// The terminal's process has exited; no more updates will come.
    fn exited(&mut self);
}

/// Fold one server `message` about `terminal` into `sink`. Messages about other
/// terminals — several panes can share one connection's worth of pushes in
/// principle — and non-feed messages (session snapshots, handshake replies) are
/// ignored. Returns whether the message was consumed by the sink.
pub fn apply_screen_message(
    message: &ServerMessage,
    terminal: TerminalId,
    sink: &mut dyn ScreenSink,
) -> bool {
    match message {
        ServerMessage::Screen {
            terminal: id,
            contents,
        } if *id == terminal => {
            sink.replace_screen(contents);
            true
        }
        ServerMessage::Output { terminal: id, data } if *id == terminal => {
            sink.apply_output(data);
            true
        }
        ServerMessage::Exited { terminal: id } if *id == terminal => {
            sink.exited();
            true
        }
        _ => false,
    }
}

/// How one server message answers a pending `Spawn`, for the handshake loop
/// that reads frames until the spawn is settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnReply {
    /// The terminal is running: its daemon-assigned id and shell pid.
    Ready { terminal: TerminalId, pid: u32 },
    /// The daemon refused the spawn.
    Rejected(String),
    /// Not an answer to the spawn (e.g. a session push) — keep reading.
    NotYet,
}

/// Interpret one server message as the answer to a pending `Spawn`.
pub fn spawn_reply(message: &ServerMessage) -> SpawnReply {
    match message {
        ServerMessage::Spawned { terminal, pid, .. } => SpawnReply::Ready {
            terminal: *terminal,
            pid: *pid,
        },
        ServerMessage::Error { message } => SpawnReply::Rejected(message.clone()),
        _ => SpawnReply::NotYet,
    }
}

/// How one server message answers a pending `Attach` for `terminal`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachReply {
    /// The attach took: the client now receives this terminal's feed. Carries
    /// the shell's pid (the spawn-less re-attach path never saw `Spawned`).
    Ready { pid: u32 },
    /// The daemon refused the attach (unknown id, or a worktree mismatch).
    Rejected(String),
    /// Not an answer to the attach — keep reading.
    NotYet,
}

/// Interpret one server message as the answer to a pending `Attach` for
/// `terminal`.
pub fn attach_reply(message: &ServerMessage, terminal: TerminalId) -> AttachReply {
    match message {
        ServerMessage::Attached { terminal: id, pid } if *id == terminal => {
            AttachReply::Ready { pid: *pid }
        }
        ServerMessage::Error { message } => AttachReply::Rejected(message.clone()),
        _ => AttachReply::NotYet,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Records every sink call so a test can assert exactly what a message did.
    #[derive(Default)]
    struct Recorded {
        replaced: Vec<Vec<u8>>,
        output: Vec<Vec<u8>>,
        exited: bool,
    }

    impl ScreenSink for Recorded {
        fn replace_screen(&mut self, contents: &[u8]) {
            self.replaced.push(contents.to_vec());
        }
        fn apply_output(&mut self, data: &[u8]) {
            self.output.push(data.to_vec());
        }
        fn exited(&mut self) {
            self.exited = true;
        }
    }

    #[test]
    fn a_screen_snapshot_replaces_the_screen() {
        let mut sink = Recorded::default();
        let consumed = apply_screen_message(
            &ServerMessage::Screen {
                terminal: 7,
                contents: b"\x1b[2Jhello".to_vec(),
            },
            7,
            &mut sink,
        );
        assert!(consumed);
        assert_eq!(sink.replaced, vec![b"\x1b[2Jhello".to_vec()]);
        assert!(sink.output.is_empty());
        assert!(!sink.exited);
    }

    #[test]
    fn an_output_delta_is_folded_in() {
        let mut sink = Recorded::default();
        assert!(apply_screen_message(
            &ServerMessage::Output {
                terminal: 7,
                data: b"world".to_vec(),
            },
            7,
            &mut sink,
        ));
        assert_eq!(sink.output, vec![b"world".to_vec()]);
    }

    #[test]
    fn an_exit_marks_the_sink_exited() {
        let mut sink = Recorded::default();
        assert!(apply_screen_message(
            &ServerMessage::Exited { terminal: 7 },
            7,
            &mut sink,
        ));
        assert!(sink.exited);
    }

    #[test]
    fn messages_about_other_terminals_are_ignored() {
        let mut sink = Recorded::default();
        assert!(!apply_screen_message(
            &ServerMessage::Screen {
                terminal: 8,
                contents: b"x".to_vec(),
            },
            7,
            &mut sink,
        ));
        assert!(!apply_screen_message(
            &ServerMessage::Output {
                terminal: 8,
                data: b"x".to_vec(),
            },
            7,
            &mut sink,
        ));
        assert!(!apply_screen_message(
            &ServerMessage::Exited { terminal: 8 },
            7,
            &mut sink,
        ));
        assert!(sink.replaced.is_empty());
        assert!(sink.output.is_empty());
        assert!(!sink.exited);
    }

    #[test]
    fn non_feed_messages_are_ignored() {
        let mut sink = Recorded::default();
        assert!(!apply_screen_message(
            &ServerMessage::Sessions {
                sessions: Vec::new()
            },
            7,
            &mut sink,
        ));
        assert!(!apply_screen_message(
            &ServerMessage::Attached {
                terminal: 7,
                pid: 1
            },
            7,
            &mut sink,
        ));
    }

    #[test]
    fn spawn_reply_matches_spawned_and_error() {
        assert_eq!(
            spawn_reply(&ServerMessage::Spawned {
                terminal: 3,
                worktree: PathBuf::from("/wt"),
                pid: 42,
            }),
            SpawnReply::Ready {
                terminal: 3,
                pid: 42
            }
        );
        assert_eq!(
            spawn_reply(&ServerMessage::Error {
                message: "no".to_string()
            }),
            SpawnReply::Rejected("no".to_string())
        );
        // A session push racing the handshake is not an answer.
        assert_eq!(
            spawn_reply(&ServerMessage::Sessions {
                sessions: Vec::new()
            }),
            SpawnReply::NotYet
        );
    }

    #[test]
    fn attach_reply_matches_only_its_terminal() {
        assert_eq!(
            attach_reply(
                &ServerMessage::Attached {
                    terminal: 3,
                    pid: 9
                },
                3
            ),
            AttachReply::Ready { pid: 9 }
        );
        // An Attached for some other terminal is not this attach's answer.
        assert_eq!(
            attach_reply(
                &ServerMessage::Attached {
                    terminal: 4,
                    pid: 9
                },
                3
            ),
            AttachReply::NotYet
        );
        assert_eq!(
            attach_reply(
                &ServerMessage::Error {
                    message: "unknown terminal".to_string()
                },
                3
            ),
            AttachReply::Rejected("unknown terminal".to_string())
        );
        assert_eq!(
            attach_reply(&ServerMessage::Exited { terminal: 3 }, 3),
            AttachReply::NotYet
        );
    }
}

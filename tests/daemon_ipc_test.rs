//! End-to-end check of the daemon IPC socket: start a real `usagi daemon`, connect
//! to its Unix domain socket, ask for the session list, and read the framed reply
//! back. This exercises the whole composition-root socket server (bind, accept,
//! per-client read/dispatch, write) that the unit tests cover only piecewise.
//!
//! Unix-only: the IPC socket is a Unix domain socket.
#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use usagi::domain::daemon_ipc::{ClientMessage, FrameDecoder, ServerMessage};
use usagi::infrastructure::daemon_ipc::{decode_message, encode_message, socket_path};

/// The compiled `usagi` binary under test.
const BIN: &str = env!("CARGO_BIN_EXE_usagi");

/// Run `usagi daemon <arg>` with `home` as $USAGI_HOME and wait for it to finish.
fn daemon_cmd(home: &Path, arg: &str) {
    let status = Command::new(BIN)
        .args(["daemon", arg])
        .env("USAGI_HOME", home)
        .status()
        .expect("running usagi daemon");
    assert!(status.success(), "usagi daemon {arg} failed");
}

/// Wait until `path` exists, up to `budget`.
fn wait_for(path: &Path, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    path.exists()
}

/// Read one framed [`ServerMessage`] from `stream`, waiting up to `budget`.
fn read_message(stream: &mut UnixStream, budget: Duration) -> ServerMessage {
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if let Some(frame) = decoder.next_frame().unwrap() {
            return decode_message(&frame).unwrap();
        }
        match stream.read(&mut buf) {
            Ok(0) => panic!("daemon closed the connection before replying"),
            Ok(n) => decoder.feed(&buf[..n]),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => panic!("reading from the daemon socket: {e}"),
        }
    }
    panic!("timed out waiting for a reply from the daemon");
}

#[test]
fn client_lists_sessions_over_the_ipc_socket() {
    let home = tempfile::tempdir().unwrap();
    let daemon_dir = home.path().join("daemon");
    let sock = socket_path(&daemon_dir);

    daemon_cmd(home.path(), "start");
    // The detached daemon binds the socket shortly after starting.
    assert!(
        wait_for(&sock, Duration::from_secs(10)),
        "daemon never created its IPC socket at {}",
        sock.display()
    );

    let outcome = std::panic::catch_unwind(|| {
        let mut stream = UnixStream::connect(&sock).expect("connecting to the daemon socket");
        stream
            .write_all(&encode_message(&ClientMessage::ListSessions).unwrap())
            .unwrap();

        // No workspaces are registered under this fresh $USAGI_HOME, so the
        // daemon reports an empty session list — proving the request reached the
        // server and a framed reply came back.
        let reply = read_message(&mut stream, Duration::from_secs(5));
        assert_eq!(
            reply,
            ServerMessage::Sessions {
                sessions: Vec::new()
            }
        );
    });

    // Always stop the daemon, even if the assertions above failed, so it does
    // not linger past the test.
    daemon_cmd(home.path(), "stop");

    if let Err(payload) = outcome {
        std::panic::resume_unwind(payload);
    }
}

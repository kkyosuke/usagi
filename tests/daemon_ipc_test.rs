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
use usagi::infrastructure::resource::process_alive;

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

/// Read one framed [`ServerMessage`] from `stream` into the caller's `decoder`,
/// waiting up to `budget`. The decoder is caller-owned so several messages read
/// in sequence over one connection never lose bytes buffered past a frame.
fn recv(stream: &mut UnixStream, decoder: &mut FrameDecoder, budget: Duration) -> ServerMessage {
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
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

/// Read one framed [`ServerMessage`] from a fresh connection (single reply).
fn read_message(stream: &mut UnixStream, budget: Duration) -> ServerMessage {
    let mut decoder = FrameDecoder::new();
    recv(stream, &mut decoder, budget)
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

#[test]
fn client_attaches_and_receives_the_terminal_screen() {
    let home = tempfile::tempdir().unwrap();
    let daemon_dir = home.path().join("daemon");
    let sock = socket_path(&daemon_dir);
    let worktree = tempfile::tempdir().unwrap();
    let worktree_path = worktree.path().to_path_buf();

    daemon_cmd(home.path(), "start");
    assert!(
        wait_for(&sock, Duration::from_secs(10)),
        "daemon never created its IPC socket"
    );

    let outcome = std::panic::catch_unwind(|| {
        let mut stream = UnixStream::connect(&sock).expect("connecting");
        let mut decoder = FrameDecoder::new();

        // Spawn a terminal, then attach to its screen feed over the same
        // connection.
        stream
            .write_all(
                &encode_message(&ClientMessage::Spawn {
                    worktree: worktree_path.clone(),
                })
                .unwrap(),
            )
            .unwrap();
        match recv(&mut stream, &mut decoder, Duration::from_secs(5)) {
            ServerMessage::Spawned { .. } => {}
            other => panic!("expected Spawned, got {other:?}"),
        }

        stream
            .write_all(
                &encode_message(&ClientMessage::Attach {
                    worktree: worktree_path.clone(),
                })
                .unwrap(),
            )
            .unwrap();
        // The daemon paints the current screen on attach: a Screen message for
        // this worktree comes back, proving the vt100 screen is streamed over IPC.
        match recv(&mut stream, &mut decoder, Duration::from_secs(5)) {
            ServerMessage::Screen { worktree, .. } => {
                assert_eq!(worktree, worktree_path, "screen was for the wrong worktree");
            }
            other => panic!("expected Screen, got {other:?}"),
        }
    });

    daemon_cmd(home.path(), "stop");

    if let Err(payload) = outcome {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn keys_written_to_a_terminal_appear_on_its_screen() {
    let home = tempfile::tempdir().unwrap();
    let daemon_dir = home.path().join("daemon");
    let sock = socket_path(&daemon_dir);
    let worktree = tempfile::tempdir().unwrap();
    let worktree_path = worktree.path().to_path_buf();

    daemon_cmd(home.path(), "start");
    assert!(
        wait_for(&sock, Duration::from_secs(10)),
        "daemon never created its IPC socket"
    );

    let outcome = std::panic::catch_unwind(|| {
        let mut stream = UnixStream::connect(&sock).expect("connecting");
        let mut decoder = FrameDecoder::new();

        let send = |stream: &mut UnixStream, msg: &ClientMessage| {
            stream.write_all(&encode_message(msg).unwrap()).unwrap();
        };

        send(
            &mut stream,
            &ClientMessage::Spawn {
                worktree: worktree_path.clone(),
            },
        );
        match recv(&mut stream, &mut decoder, Duration::from_secs(5)) {
            ServerMessage::Spawned { .. } => {}
            other => panic!("expected Spawned, got {other:?}"),
        }
        send(
            &mut stream,
            &ClientMessage::Attach {
                worktree: worktree_path.clone(),
            },
        );

        // Type a command that prints a distinctive marker, then read screen
        // updates until the marker shows up — proving input reached the
        // daemon-owned terminal and its output streamed back.
        send(
            &mut stream,
            &ClientMessage::Keys {
                worktree: worktree_path.clone(),
                data: b"printf usagi-keys-ok\n".to_vec(),
            },
        );

        let marker = b"usagi-keys-ok";
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = false;
        while Instant::now() < deadline {
            if let ServerMessage::Screen { contents, .. } =
                recv(&mut stream, &mut decoder, Duration::from_secs(5))
            {
                if contents
                    .windows(marker.len())
                    .any(|window| window == marker)
                {
                    seen = true;
                    break;
                }
            }
        }
        assert!(
            seen,
            "the typed marker never appeared on the terminal screen"
        );
    });

    daemon_cmd(home.path(), "stop");

    if let Err(payload) = outcome {
        std::panic::resume_unwind(payload);
    }
}

/// Poll `cond` until it holds or `budget` elapses; returns whether it held.
fn wait_until(mut cond: impl FnMut() -> bool, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    cond()
}

#[test]
fn daemon_owned_terminal_survives_client_disconnect() {
    let home = tempfile::tempdir().unwrap();
    let daemon_dir = home.path().join("daemon");
    let sock = socket_path(&daemon_dir);
    // The spawned shell runs with this directory as its cwd.
    let worktree = tempfile::tempdir().unwrap();

    daemon_cmd(home.path(), "start");
    assert!(
        wait_for(&sock, Duration::from_secs(10)),
        "daemon never created its IPC socket"
    );

    let outcome = std::panic::catch_unwind(|| {
        // A client asks the daemon to spawn a terminal, then disconnects.
        let pid = {
            let mut client = UnixStream::connect(&sock).expect("connecting client A");
            client
                .write_all(
                    &encode_message(&ClientMessage::Spawn {
                        worktree: worktree.path().to_path_buf(),
                    })
                    .unwrap(),
                )
                .unwrap();
            match read_message(&mut client, Duration::from_secs(5)) {
                ServerMessage::Spawned { pid, .. } => pid,
                other => panic!("expected Spawned, got {other:?}"),
            }
            // `client` drops here — the connection closes.
        };
        assert!(pid != 0, "daemon reported no pid for the spawned terminal");

        // The daemon owns the process, so it stays alive after the client that
        // requested it has gone. (Give the daemon a tick to notice the drop.)
        std::thread::sleep(Duration::from_millis(700));
        assert!(
            process_alive(pid),
            "the daemon-owned terminal (pid {pid}) died when its client disconnected"
        );

        // A fresh client kills it, and the process goes away.
        let mut killer = UnixStream::connect(&sock).expect("connecting client B");
        killer
            .write_all(
                &encode_message(&ClientMessage::Kill {
                    worktree: worktree.path().to_path_buf(),
                })
                .unwrap(),
            )
            .unwrap();
        match read_message(&mut killer, Duration::from_secs(5)) {
            ServerMessage::Killed { .. } => {}
            other => panic!("expected Killed, got {other:?}"),
        }
        assert!(
            wait_until(|| !process_alive(pid), Duration::from_secs(5)),
            "the terminal (pid {pid}) was not killed"
        );
    });

    daemon_cmd(home.path(), "stop");

    if let Err(payload) = outcome {
        std::panic::resume_unwind(payload);
    }
}

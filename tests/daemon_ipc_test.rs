//! End-to-end check of the daemon IPC socket: start a real `usagi daemon`, connect
//! to its Unix domain socket, and drive the terminal protocol — spawn, attach,
//! keys, detach, kill — reading the framed replies back. This exercises the whole
//! composition-root socket server (bind, accept, per-client read/dispatch, write,
//! output streaming) that the unit tests cover only piecewise.
//!
//! Unix-only: the IPC socket is a Unix domain socket.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use usagi::domain::daemon_ipc::{ClientMessage, FrameDecoder, ServerMessage, TerminalId};
use usagi::infrastructure::daemon_ipc::{decode_message, encode_message, socket_path};
use usagi::infrastructure::daemon_store;
use usagi::infrastructure::resource::process_alive;

/// The compiled `usagi` binary under test.
const BIN: &str = env!("CARGO_BIN_EXE_usagi");

fn daemon_e2e_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Run `usagi daemon <arg>` with `home` as $USAGI_HOME and wait for it to finish.
fn daemon_cmd(home: &Path, arg: &str) {
    let status = Command::new(BIN)
        .args(["daemon", arg])
        .current_dir(home)
        .env("USAGI_HOME", home)
        .env_remove("LLVM_PROFILE_FILE")
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

/// Coverage instrumentation slows the real daemon process enough that the
/// tight E2E socket budgets can trip before the daemon replies.
fn e2e_budget(seconds: u64) -> Duration {
    let multiplier = if std::env::var_os("LLVM_PROFILE_FILE").is_some() {
        12
    } else {
        1
    };
    Duration::from_secs(seconds * multiplier)
}

/// Connect to the daemon socket, retrying briefly: on a loaded machine (several
/// of these tests spawn daemons in parallel, more so under coverage
/// instrumentation) the socket file can be observable an instant before the
/// freshly exec'd daemon is accepting, which surfaces as `ConnectionRefused`.
fn connect(sock: &Path) -> UnixStream {
    let deadline = Instant::now() + e2e_budget(10);
    loop {
        match UnixStream::connect(sock) {
            Ok(mut stream) => {
                let build = usagi::infrastructure::daemon_client::build_identity_for(Path::new(
                    env!("CARGO_BIN_EXE_usagi"),
                ))
                .unwrap();
                send(
                    &mut stream,
                    &ClientMessage::Hello {
                        build: build.clone(),
                    },
                );
                let mut decoder = FrameDecoder::new();
                assert_eq!(
                    recv(&mut stream, &mut decoder, e2e_budget(5)),
                    ServerMessage::Hello { build }
                );
                return stream;
            }
            Err(e) if Instant::now() < deadline => {
                let _ = e;
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("connecting to the daemon socket: {e}"),
        }
    }
}

/// A `Spawn` message for `worktree` with the defaults the tests use.
fn spawn_message(worktree: &Path) -> ClientMessage {
    ClientMessage::Spawn {
        worktree: worktree.to_path_buf(),
        command: None,
        env: BTreeMap::new(),
        cols: 80,
        rows: 24,
        scrollback: 200,
    }
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

/// Send `message` on `stream`, panicking on a write failure.
fn send(stream: &mut UnixStream, message: &ClientMessage) {
    stream.write_all(&encode_message(message).unwrap()).unwrap();
}

/// Spawn a terminal over `stream` and return its id and pid.
fn spawn_terminal(
    stream: &mut UnixStream,
    decoder: &mut FrameDecoder,
    worktree: &Path,
) -> (TerminalId, u32) {
    send(stream, &spawn_message(worktree));
    match recv(stream, decoder, e2e_budget(5)) {
        ServerMessage::Spawned { terminal, pid, .. } => (terminal, pid),
        other => panic!("expected Spawned, got {other:?}"),
    }
}

/// Attach to `terminal` over `stream`, asserting the `Attached` reply and the
/// initial `Screen` snapshot that follows it.
fn attach_terminal(
    stream: &mut UnixStream,
    decoder: &mut FrameDecoder,
    worktree: &Path,
    terminal: TerminalId,
) -> Vec<u8> {
    send(
        stream,
        &ClientMessage::Attach {
            terminal,
            worktree: worktree.to_path_buf(),
        },
    );
    match recv(stream, decoder, e2e_budget(5)) {
        ServerMessage::Attached { terminal: id, .. } => {
            assert_eq!(id, terminal, "attached to the wrong terminal");
        }
        other => panic!("expected Attached, got {other:?}"),
    }
    // The daemon paints the current screen right after the attach reply.
    match recv(stream, decoder, e2e_budget(5)) {
        ServerMessage::Screen {
            terminal: id,
            contents,
            ..
        } => {
            assert_eq!(id, terminal, "screen was for the wrong terminal");
            contents
        }
        other => panic!("expected Screen, got {other:?}"),
    }
}

/// Keep reading screen updates for `terminal` until `marker` appears in one,
/// or `budget` runs out. Both the full-snapshot and the raw-delta forms count —
/// which one arrives depends on timing (a resync vs. an incremental push).
fn wait_for_marker(
    stream: &mut UnixStream,
    decoder: &mut FrameDecoder,
    terminal: TerminalId,
    marker: &[u8],
    budget: Duration,
) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        let bytes = match recv(stream, decoder, e2e_budget(5)) {
            ServerMessage::Screen {
                terminal: id,
                contents,
                ..
            } if id == terminal => contents,
            ServerMessage::Output { terminal: id, data } if id == terminal => data,
            _ => continue,
        };
        if bytes.windows(marker.len()).any(|window| window == marker) {
            return true;
        }
    }
    false
}

#[test]
fn client_lists_sessions_over_the_ipc_socket() {
    let _guard = daemon_e2e_guard();
    let home = tempfile::tempdir().unwrap();
    let daemon_dir = home.path().join("daemon");
    let sock = socket_path(&daemon_dir);

    let outcome = std::panic::catch_unwind(|| {
        daemon_cmd(home.path(), "start");
        // The detached daemon binds the socket shortly after starting.
        assert!(
            wait_for(&sock, e2e_budget(10)),
            "daemon never created its IPC socket at {}",
            sock.display()
        );
        let mut stream = connect(&sock);
        send(&mut stream, &ClientMessage::ListSessions);

        // No workspaces are registered under this fresh $USAGI_HOME, so the
        // daemon reports an empty session list — proving the request reached the
        // server and a framed reply came back.
        let reply = read_message(&mut stream, e2e_budget(5));
        assert_eq!(
            reply,
            ServerMessage::Sessions {
                sessions: Vec::new()
            }
        );
    });

    // Always stop the daemon, even if the assertions above failed, so it does
    // not linger past the test.
    stop_daemon(home.path(), &daemon_dir);

    if let Err(payload) = outcome {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn client_attaches_and_receives_the_terminal_screen() {
    let _guard = daemon_e2e_guard();
    let home = tempfile::tempdir().unwrap();
    let daemon_dir = home.path().join("daemon");
    let sock = socket_path(&daemon_dir);
    let worktree = tempfile::tempdir().unwrap();

    let outcome = std::panic::catch_unwind(|| {
        daemon_cmd(home.path(), "start");
        assert!(
            wait_for(&sock, e2e_budget(10)),
            "daemon never created its IPC socket"
        );
        let mut stream = connect(&sock);
        let mut decoder = FrameDecoder::new();

        // Spawn a terminal, then attach to its screen feed over the same
        // connection: Attached + the initial Screen snapshot come back, proving
        // the vt100 screen is streamed over IPC.
        let (terminal, _) = spawn_terminal(&mut stream, &mut decoder, worktree.path());
        attach_terminal(&mut stream, &mut decoder, worktree.path(), terminal);

        // Attaching with the wrong worktree is refused: a stale persisted id
        // can never latch onto another worktree's terminal.
        let elsewhere = tempfile::tempdir().unwrap();
        send(
            &mut stream,
            &ClientMessage::Attach {
                terminal,
                worktree: elsewhere.path().to_path_buf(),
            },
        );
        let deadline = Instant::now() + e2e_budget(5);
        loop {
            match recv(&mut stream, &mut decoder, e2e_budget(5)) {
                ServerMessage::Error { message } => {
                    assert!(
                        message.contains("no daemon terminal"),
                        "odd error: {message}"
                    );
                    break;
                }
                // Terminal output may still be in flight ahead of the reply.
                ServerMessage::Screen { .. } | ServerMessage::Output { .. }
                    if Instant::now() < deadline => {}
                other => panic!("expected Error for a worktree mismatch, got {other:?}"),
            }
        }
    });

    stop_daemon(home.path(), &daemon_dir);

    if let Err(payload) = outcome {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn keys_written_to_a_terminal_appear_on_its_screen() {
    let _guard = daemon_e2e_guard();
    let home = tempfile::tempdir().unwrap();
    let daemon_dir = home.path().join("daemon");
    let sock = socket_path(&daemon_dir);
    let worktree = tempfile::tempdir().unwrap();

    let outcome = std::panic::catch_unwind(|| {
        daemon_cmd(home.path(), "start");
        assert!(
            wait_for(&sock, e2e_budget(10)),
            "daemon never created its IPC socket"
        );
        let mut stream = connect(&sock);
        let mut decoder = FrameDecoder::new();

        let (terminal, _) = spawn_terminal(&mut stream, &mut decoder, worktree.path());
        attach_terminal(&mut stream, &mut decoder, worktree.path(), terminal);

        // Type a command that prints a distinctive marker, then read screen
        // updates until the marker shows up — proving input reached the
        // daemon-owned terminal and its output streamed back.
        send(
            &mut stream,
            &ClientMessage::Keys {
                terminal,
                data: b"printf usagi-keys-ok\n".to_vec(),
            },
        );
        assert!(
            wait_for_marker(
                &mut stream,
                &mut decoder,
                terminal,
                b"usagi-keys-ok",
                e2e_budget(10),
            ),
            "the typed marker never appeared on the terminal screen"
        );
    });

    stop_daemon(home.path(), &daemon_dir);

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

/// Stop a test daemon and wait for the detached process to exit before its
/// temporary data directory is dropped. If graceful shutdown times out, kill
/// the recorded pid so a failed test cannot leak an orphan onto the host.
fn stop_daemon(home: &Path, daemon_dir: &Path) {
    let pid = daemon_store::read(daemon_dir)
        .ok()
        .flatten()
        .map(|record| record.pid);

    let _ = Command::new(BIN)
        .args(["daemon", "stop"])
        .current_dir(home)
        .env("USAGI_HOME", home)
        .env_remove("LLVM_PROFILE_FILE")
        .status();

    if let Some(pid) = pid {
        if !wait_until(|| !process_alive(pid), e2e_budget(5)) {
            // SAFETY: `pid` came from this test's private daemon record. SIGKILL
            // is the final fallback after its graceful shutdown budget elapsed.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
            let _ = wait_until(|| !process_alive(pid), e2e_budget(5));
        }
    }
}

#[test]
fn daemon_owned_terminal_survives_detach_and_disconnect() {
    let _guard = daemon_e2e_guard();
    let home = tempfile::tempdir().unwrap();
    let daemon_dir = home.path().join("daemon");
    let sock = socket_path(&daemon_dir);
    // The spawned shell runs with this directory as its cwd.
    let worktree = tempfile::tempdir().unwrap();

    let outcome = std::panic::catch_unwind(|| {
        daemon_cmd(home.path(), "start");
        assert!(
            wait_for(&sock, e2e_budget(10)),
            "daemon never created its IPC socket"
        );
        // A client spawns a terminal, attaches, then detaches (the TUI's
        // Ctrl-O / quit path) and disconnects entirely.
        let (terminal, pid) = {
            let mut client = connect(&sock);
            let mut decoder = FrameDecoder::new();
            let (terminal, pid) = spawn_terminal(&mut client, &mut decoder, worktree.path());
            attach_terminal(&mut client, &mut decoder, worktree.path(), terminal);
            send(&mut client, &ClientMessage::Detach { terminal });
            (terminal, pid)
            // `client` drops here — the connection closes.
        };
        assert!(pid != 0, "daemon reported no pid for the spawned terminal");

        // The daemon owns the process, so it stays alive after the client that
        // viewed it detached and disconnected — this is "close the TUI, the
        // agent keeps running". (Give the daemon a tick to notice the drop.)
        std::thread::sleep(Duration::from_millis(700));
        assert!(
            process_alive(pid),
            "the daemon-owned terminal (pid {pid}) died when its client detached"
        );

        // A fresh client re-attaches by id — the restore path a reopened TUI
        // takes — and sees the terminal's screen again.
        let mut returning = connect(&sock);
        let mut decoder = FrameDecoder::new();
        attach_terminal(&mut returning, &mut decoder, worktree.path(), terminal);

        // A kill by id tears the terminal down for real.
        send(&mut returning, &ClientMessage::Kill { terminal });
        let deadline = Instant::now() + e2e_budget(5);
        loop {
            match recv(&mut returning, &mut decoder, e2e_budget(5)) {
                ServerMessage::Killed { terminal: id } => {
                    assert_eq!(id, terminal);
                    break;
                }
                // Screen/Output pushes may still be in flight ahead of the reply.
                _ if Instant::now() < deadline => continue,
                other => panic!("expected Killed, got {other:?}"),
            }
        }
        assert!(
            wait_until(|| !process_alive(pid), e2e_budget(5)),
            "the terminal (pid {pid}) was not killed"
        );
    });

    stop_daemon(home.path(), &daemon_dir);

    if let Err(payload) = outcome {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn spawn_runs_the_given_command_and_reports_its_exit() {
    let _guard = daemon_e2e_guard();
    if std::env::var_os("LLVM_PROFILE_FILE").is_some() {
        eprintln!("skipping command-spawn daemon E2E under cargo-llvm-cov");
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let daemon_dir = home.path().join("daemon");
    let sock = socket_path(&daemon_dir);
    let worktree = tempfile::tempdir().unwrap();

    let outcome = std::panic::catch_unwind(|| {
        daemon_cmd(home.path(), "start");
        assert!(
            wait_for(&sock, e2e_budget(10)),
            "daemon never created its IPC socket"
        );
        let mut stream = connect(&sock);
        let mut decoder = FrameDecoder::new();

        // Spawn with an opening command (the agent-launch path) that prints a
        // marker and exits, and an env var the command echoes — proving both
        // ride the Spawn message into the daemon-owned shell.
        send(
            &mut stream,
            &ClientMessage::Spawn {
                worktree: worktree.path().to_path_buf(),
                command: Some("sleep 1; printf usagi-cmd-$USAGI_E2E_MARKER; sleep 1".to_string()),
                env: [("USAGI_E2E_MARKER".to_string(), "env-ok".to_string())].into(),
                cols: 80,
                rows: 24,
                scrollback: 200,
            },
        );
        let terminal = match recv(&mut stream, &mut decoder, e2e_budget(5)) {
            ServerMessage::Spawned { terminal, .. } => terminal,
            other => panic!("expected Spawned, got {other:?}"),
        };
        let initial_screen = attach_terminal(&mut stream, &mut decoder, worktree.path(), terminal);
        let marker = b"usagi-cmd-env-ok";
        assert!(
            initial_screen
                .windows(marker.len())
                .any(|window| window == marker)
                || wait_for_marker(&mut stream, &mut decoder, terminal, marker, e2e_budget(10),),
            "the opening command's output never appeared"
        );

        // The command exits when it is done, and the daemon reports the death
        // to its attachers — the signal a TUI pane closes on.
        let deadline = Instant::now() + e2e_budget(10);
        loop {
            match recv(&mut stream, &mut decoder, e2e_budget(5)) {
                ServerMessage::Exited { terminal: id } => {
                    assert_eq!(id, terminal);
                    break;
                }
                _ if Instant::now() < deadline => continue,
                other => panic!("expected Exited, got {other:?}"),
            }
        }
    });

    stop_daemon(home.path(), &daemon_dir);

    if let Err(payload) = outcome {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn terminal_client_rejects_a_daemon_from_another_executable_generation() {
    let _guard = daemon_e2e_guard();
    let home = tempfile::tempdir().unwrap();
    let daemon_dir = home.path().join("daemon");
    let sock = socket_path(&daemon_dir);
    let worktree = tempfile::tempdir().unwrap();

    let client_build = usagi::infrastructure::daemon_client::build_identity().unwrap();
    let daemon_build = usagi::infrastructure::daemon_client::build_identity_for(Path::new(env!(
        "CARGO_BIN_EXE_usagi"
    )))
    .unwrap();
    assert_ne!(client_build, daemon_build, "the test needs two executables");

    let outcome = std::panic::catch_unwind(|| {
        daemon_cmd(home.path(), "start");
        assert!(
            wait_for(&sock, e2e_budget(10)),
            "daemon never created its IPC socket"
        );

        let mut before_spawn_called = false;
        let error = usagi::infrastructure::daemon_client::DaemonTerminal::spawn_after_build_check(
            &daemon_dir,
            worktree.path(),
            24,
            80,
            Some("printf should-not-run"),
            200,
            &BTreeMap::new(),
            &mut || before_spawn_called = true,
        )
        .err()
        .expect("a different daemon build must be rejected");
        assert!(
            error.to_string().contains("daemon build mismatch"),
            "unexpected error: {error:#}"
        );
        assert!(error
            .downcast_ref::<usagi::infrastructure::daemon_client::DaemonBuildHandshakeError>()
            .is_some());
        assert!(
            !before_spawn_called,
            "a rejected daemon build must not clear the existing agent phase"
        );

        // Restore must be able to distinguish the same pre-attach failure from
        // an ordinary missing terminal. Otherwise it would resume the recorded
        // agent locally while the old daemon's process is still running.
        let attach_error = usagi::infrastructure::daemon_client::DaemonTerminal::attach(
            &daemon_dir,
            worktree.path(),
            1,
            24,
            80,
            200,
        )
        .err()
        .expect("restore attach to a different daemon build must be rejected");
        assert!(attach_error
            .downcast_ref::<usagi::infrastructure::daemon_client::DaemonBuildHandshakeError>()
            .is_some());

        // A pre-handshake client (the previous protocol generation) is refused
        // server-side as well. Its Spawn payload must not degrade into a plain
        // shell on the newer daemon.
        let mut old_client = UnixStream::connect(&sock).unwrap();
        send(&mut old_client, &spawn_message(worktree.path()));
        match read_message(&mut old_client, e2e_budget(5)) {
            ServerMessage::Error { message } => assert!(message.contains("handshake required")),
            other => panic!("expected build-handshake error, got {other:?}"),
        }
        assert!(
            usagi::infrastructure::daemon_terminals_store::read(&daemon_dir)
                .unwrap()
                .is_empty(),
            "the rejected client must not spawn a plain terminal"
        );
    });

    stop_daemon(home.path(), &daemon_dir);

    if let Err(payload) = outcome {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn daemon_exits_when_its_data_dir_disappears() {
    let _guard = daemon_e2e_guard();
    let home = tempfile::tempdir().unwrap();
    let daemon_dir = home.path().join("daemon");
    let sock = socket_path(&daemon_dir);

    let outcome = std::panic::catch_unwind(|| {
        daemon_cmd(home.path(), "start");
        assert!(
            wait_for(&sock, e2e_budget(10)),
            "daemon never created its IPC socket"
        );
        let pid = daemon_store::read(&daemon_dir)
            .unwrap()
            .expect("daemon record")
            .pid;

        std::fs::remove_dir_all(&daemon_dir).unwrap();
        assert!(
            wait_until(|| !process_alive(pid), e2e_budget(2)),
            "daemon {pid} did not exit after its data directory disappeared"
        );
    });

    stop_daemon(home.path(), &daemon_dir);
    if let Err(payload) = outcome {
        std::panic::resume_unwind(payload);
    }
}

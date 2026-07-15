//! Root daemon generic (login-shell) terminal IPC regression.
//!
//! This starts the shipping composition root (`usagi daemon serve`) and drives a
//! real shell PTY purely over the Unix socket. Unlike the Agent e2e it needs no
//! fixture executable: the trusted `login-shell` profile resolves to the system
//! `/bin/sh` rooted at the daemon's repository. The test proves the full managed
//! flow the integration slice (#270) connects — create a managed session, launch
//! a daemon-owned generic terminal in that session's fenced scope, attach, type a
//! command, and read the command's output back from the daemon's replay buffer.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use usagi_core::domain::id::{OperationId, SessionId, TerminalRef, WorkspaceId, WorktreeId};
use usagi_core::domain::terminal_launch::{
    TerminalLaunchRequest, TerminalLaunchScope, TerminalProfileId,
};
use usagi_core::infrastructure::ipc::ErrorCode;
use usagi_core::usecase::client::{
    ClientError, ClientPolicy, DaemonClient, DaemonReply, DaemonRequest, IpcClient, SessionAction,
    TerminalAction, TerminalGeometry, TerminalLaunchIntent, TerminalRequest,
};
use usagi_daemon::infrastructure::unix_transport::connect_current;

fn short_dir(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in("/tmp")
        .expect("short Unix socket path")
}

fn channel_data_dir(home: &Path) -> PathBuf {
    if cfg!(debug_assertions) {
        home.join("development")
    } else {
        home.to_path_buf()
    }
}

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .expect("git must start for the temporary fixture repository");
    assert!(status.success(), "git {args:?} failed");
}

/// A git repository the daemon serves from. Its single tracked file is the
/// marker `ls` looks for, so the shell PTY output is asserted without any
/// custom fixture binary.
fn fixture_repo() -> tempfile::TempDir {
    let repo = short_dir("usagi-term-repo-");
    git(repo.path(), &["init", "-q"]);
    git(
        repo.path(),
        &["config", "user.email", "term-e2e@example.test"],
    );
    git(repo.path(), &["config", "user.name", "Terminal E2E"]);
    fs::write(repo.path().join("USAGI_MARKER.txt"), "fixture\n").unwrap();
    git(repo.path(), &["add", "USAGI_MARKER.txt"]);
    git(repo.path(), &["commit", "-qm", "fixture"]);
    repo
}

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_daemon(repo: &Path, home: &Path) -> Daemon {
    let data_dir = channel_data_dir(home);
    fs::create_dir(&data_dir).expect("daemon data directory exists before serve");
    fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_usagi"))
        .args(["daemon", "serve"])
        .current_dir(repo)
        .env("USAGI_HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("root daemon starts");
    Daemon { child }
}

fn client(data_dir: &Path) -> IpcClient<std::os::unix::net::UnixStream> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(stream) = connect_current(data_dir) {
            return IpcClient::connect(
                stream,
                "generic-terminal-e2e".into(),
                OperationId::new().to_string(),
                ClientPolicy::cli(),
            )
            .expect("Unix IPC handshake succeeds");
        }
        assert!(
            Instant::now() < deadline,
            "daemon did not publish its socket"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// Create a managed session and return its stable, fully fenced scope. A generic
/// terminal client supplies this whole scope on launch (the daemon does not
/// rediscover it by name).
fn available_scope(client: &mut impl DaemonClient) -> (WorkspaceId, SessionId, WorktreeId) {
    let reply = client
        .request(DaemonRequest::Session {
            action: SessionAction::Create,
            operation_id: OperationId::new().to_string(),
            payload: serde_json::json!({"name": "term-e2e"}),
        })
        .expect("session fixture is created through root IPC");
    let body = match reply {
        DaemonReply::Accepted { body, .. } | DaemonReply::Ok(body) => body,
    };
    let workspace = serde_json::from_value(body["workspace_id"].clone()).unwrap();
    let session = body["sessions"]
        .as_array()
        .expect("session snapshot array")
        .iter()
        .find(|session| session["name"] == "term-e2e")
        .expect("created session is present");
    (
        workspace,
        serde_json::from_value(session["session_id"].clone()).unwrap(),
        serde_json::from_value(session["worktree_id"].clone()).unwrap(),
    )
}

fn launch_request(
    profile: &str,
    workspace: WorkspaceId,
    session: SessionId,
    worktree: WorktreeId,
) -> TerminalLaunchRequest {
    TerminalLaunchRequest {
        profile_id: TerminalProfileId::new(profile).unwrap(),
        scope: TerminalLaunchScope {
            workspace_id: workspace,
            session_id: Some(session),
            worktree_id: worktree,
        },
    }
}

fn launch_terminal(
    client: &mut impl DaemonClient,
    request: TerminalLaunchRequest,
) -> Result<TerminalRef, ClientError> {
    let reply = client.request(DaemonRequest::Terminal {
        action: TerminalAction::Launch,
        payload: serde_json::to_value(TerminalRequest::Launch {
            intent: TerminalLaunchIntent {
                request,
                geometry: TerminalGeometry { cols: 80, rows: 24 },
            },
        })
        .unwrap(),
    })?;
    let DaemonReply::Ok(body) = reply else {
        panic!("generic terminal launch is a synchronous Ok, not an admission");
    };
    Ok(serde_json::from_value(body["terminal"].clone()).unwrap())
}

fn attach(client: &mut impl DaemonClient, terminal: &TerminalRef) -> u64 {
    let reply = client
        .request(DaemonRequest::Terminal {
            action: TerminalAction::Attach,
            payload: serde_json::to_value(TerminalRequest::Attach {
                terminal: terminal.clone(),
            })
            .unwrap(),
        })
        .expect("terminal attaches through root IPC");
    let DaemonReply::Ok(body) = reply else {
        panic!("terminal attach must not be an operation admission");
    };
    body["subscription"].as_u64().expect("subscription id")
}

fn resync(client: &mut impl DaemonClient, terminal: &TerminalRef) -> serde_json::Value {
    let reply = client
        .request(DaemonRequest::Terminal {
            action: TerminalAction::Resync,
            payload: serde_json::to_value(TerminalRequest::Resync {
                terminal: terminal.clone(),
            })
            .unwrap(),
        })
        .expect("terminal resyncs through root IPC");
    let DaemonReply::Ok(snapshot) = reply else {
        panic!("resync returns a snapshot");
    };
    snapshot
}

/// The replay buffer, decoded to text, so a test can wait for shell output.
fn replay_text(client: &mut impl DaemonClient, terminal: &TerminalRef) -> String {
    let snapshot = resync(client, terminal);
    let bytes: Vec<u8> = snapshot["replay"]
        .as_array()
        .expect("replay byte array")
        .iter()
        .map(|value| u8::try_from(value.as_u64().expect("replay byte")).expect("byte in range"))
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn input(
    client: &mut impl DaemonClient,
    terminal: &TerminalRef,
    subscription: u64,
    seq: u64,
    bytes: &[u8],
) {
    client
        .request(DaemonRequest::Terminal {
            action: TerminalAction::Input,
            payload: serde_json::to_value(TerminalRequest::Input {
                terminal: terminal.clone(),
                subscription,
                input_seq: seq,
                bytes: bytes.to_vec(),
            })
            .unwrap(),
        })
        .expect("typed input reaches the shell PTY");
}

fn wait_for_output(client: &mut impl DaemonClient, terminal: &TerminalRef, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let text = replay_text(client, terminal);
        if text.contains(needle) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "shell PTY did not emit {needle:?}; saw: {text:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// A managed session can launch a real login-shell PTY, receive typed input, and
/// stream the command's output back — the concrete "type `ls`, see its output"
/// contract, verified end-to-end over the Unix socket.
#[test]
fn managed_session_login_shell_echoes_typed_command_output() {
    let repo = fixture_repo();
    let home = short_dir("usagi-");
    let _daemon = start_daemon(repo.path(), home.path());
    let data_dir = channel_data_dir(home.path());
    let mut client = client(&data_dir);

    let (workspace, session, worktree) = available_scope(&mut client);
    let terminal = launch_terminal(
        &mut client,
        launch_request("login-shell", workspace, session, worktree),
    )
    .expect("trusted login-shell launches");
    // The launched terminal is fenced to the requesting session scope.
    assert_eq!(terminal.session_id, Some(session));
    assert_eq!(terminal.workspace_id, workspace);

    let subscription = attach(&mut client, &terminal);

    // Give the shell a moment to reach its first prompt, then type a command and
    // Enter, exactly as a TUI terminal tab does.
    thread::sleep(Duration::from_millis(150));
    input(
        &mut client,
        &terminal,
        subscription,
        0,
        b"echo USAGI_E2E_MARKER\n",
    );

    // The command's stdout comes back through the daemon replay journal.
    let after_echo = wait_for_output(&mut client, &terminal, "USAGI_E2E_MARKER");
    assert!(
        after_echo.contains("USAGI_E2E_MARKER"),
        "echo output missing: {after_echo:?}"
    );

    // A second command (`ls`) lists the repository the shell was rooted in.
    input(&mut client, &terminal, subscription, 1, b"ls\n");
    let after_ls = wait_for_output(&mut client, &terminal, "USAGI_MARKER.txt");
    assert!(
        after_ls.contains("USAGI_MARKER.txt"),
        "ls output missing the repository file: {after_ls:?}"
    );

    // Typing `exit` closes the shell; the daemon reaps the child and records a
    // durable exit status instead of leaving the terminal Running forever.
    input(&mut client, &terminal, subscription, 2, b"exit\n");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = resync(&mut client, &terminal);
        if !snapshot["exited"].is_null() {
            assert_eq!(snapshot["exited"], 0, "shell exit status: {snapshot:?}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "shell PTY was not reaped after exit: {snapshot:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// Only the trusted, code-defined profile may launch. A client-selected unknown
/// profile is a typed, safe rejection — no PTY spawn, no client fallback.
#[test]
fn untrusted_terminal_profile_is_rejected() {
    let repo = fixture_repo();
    let home = short_dir("usagi-");
    let _daemon = start_daemon(repo.path(), home.path());
    let data_dir = channel_data_dir(home.path());
    let mut client = client(&data_dir);

    let (workspace, session, worktree) = available_scope(&mut client);
    let error = launch_terminal(
        &mut client,
        launch_request("untrusted-shell", workspace, session, worktree),
    )
    .expect_err("an unknown profile cannot launch");
    let ClientError::Protocol(error) = error else {
        panic!("profile rejection must be a typed daemon protocol error: {error:?}");
    };
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    // The rejection must not leak the resolved program, argv, or path.
    for private in ["/bin/sh", "argv", "PATH"] {
        assert!(
            !error.message.contains(private),
            "leaked {private}: {error:?}"
        );
    }
}

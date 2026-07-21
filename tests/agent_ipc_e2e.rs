//! Root daemon Agent IPC regression with a fixture Codex executable.
//!
//! This deliberately starts the shipping composition root and talks only over
//! its Unix socket.  The fixture is placed on PATH, so neither a real Codex
//! installation nor credentials are needed.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use usagi_core::domain::agent::AgentProfileId;
use usagi_core::domain::id::{OperationId, SessionId, TerminalRef, WorkspaceId, WorktreeId};
use usagi_core::domain::terminal_launch::{
    TerminalLaunchRequest, TerminalLaunchScope, TerminalProfileId,
};
use usagi_core::infrastructure::ipc::ErrorCode;
use usagi_core::usecase::client::{
    AgentLaunchIntent, ClientError, ClientPolicy, DaemonClient, DaemonReply, DaemonRequest,
    IpcClient, SessionAction, TerminalAction, TerminalGeometry, TerminalLaunchIntent,
    TerminalRequest,
};
use usagi_daemon::infrastructure::unix_transport::connect_current;

// The daemon is an instrumented child when cargo-llvm-cov runs this suite.
// Starting it can take longer than the normal test-runner budget on a loaded
// CI worker, even though it is healthy. Keep the readiness deadline above
// that startup variance; connection failures still fail deterministically.
const DAEMON_READINESS_TIMEOUT: Duration = Duration::from_secs(60);

/// Each case starts the shipping daemon binary. Serialising those startups
/// avoids starving a loaded CI worker and turning socket publication into a
/// spurious readiness timeout.
static DAEMON_START_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn short_dir(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in("/tmp")
        .expect("short Unix socket path")
}

fn channel_data_dir(home: &Path) -> PathBuf {
    usagi_core::infrastructure::paths::channel_data_dir(home)
}

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        // Hooks export these for their own worktree. Fixture repositories
        // must not inherit them, or parallel coverage runs mutate the parent.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .status()
        .expect("git must start for the temporary fixture repository");
    assert!(status.success(), "git {args:?} failed");
}

fn fixture_repo() -> tempfile::TempDir {
    let repo = short_dir("usagi-agent-repo-");
    git(repo.path(), &["init", "-q"]);
    git(
        repo.path(),
        &["config", "user.email", "agent-e2e@example.test"],
    );
    git(repo.path(), &["config", "user.name", "Agent E2E"]);
    fs::write(repo.path().join("README.md"), "fixture\n").unwrap();
    git(repo.path(), &["add", "README.md"]);
    git(repo.path(), &["commit", "-qm", "fixture"]);
    repo
}

fn write_codex(bin: &Path, count: &Path, ready_status: i32) {
    fs::create_dir_all(bin).unwrap();
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = login ] && [ \"$2\" = status ]; then exit {ready_status}; fi\nif [ \"${{USAGI_PTY_SENTINEL+set}}\" = set ]; then exit 9; fi\nresuming=false\nfor argument in \"$@\"; do if [ \"$argument\" = resume ]; then resuming=true; fi; done\nif [ \"$resuming\" = false ]; then\n  printf '%s' '{{\"session_id\":\"fixture-codex-session\",\"transcript_path\":\"/must/not/be/read.jsonl\",\"cwd\":\"/fixture\",\"hook_event_name\":\"SessionStart\",\"model\":\"fixture\"}}' | \"{}\" codex-session-capture || exit 8\nfi\nprintf '%s\\n' spawn >> \"{}\"\nprintf 'ready\\n'\nIFS= read line || exit 0\nprintf 'input:%s\\n' \"$line\"\n",
        env!("CARGO_BIN_EXE_usagi"),
        count.display(),
    );
    let path = bin.join("codex");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn write_shell(path: &Path, count: &Path) {
    let script = format!(
        "#!/bin/sh\nif [ \"${{USAGI_PTY_SENTINEL+set}}\" = set ]; then exit 9; fi\nprintf '%s\\n' spawn >> \"{}\"\nprintf 'shell-ready\\n'\nIFS= read line || exit 0\nprintf 'shell-input:%s\\n' \"$line\"\nexit 0\n",
        count.display()
    );
    fs::write(path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
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

fn start_daemon(repo: &Path, home: &Path, path: &Path, shell: Option<&Path>) -> Daemon {
    let data_dir = channel_data_dir(home);
    fs::create_dir(&data_dir).expect("daemon data directory exists before serve");
    fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let fixture_path = format!("{}:/usr/bin:/bin", path.display());
    let mut command = Command::new(env!("CARGO_BIN_EXE_usagi"));
    command
        .args(["daemon", "serve"])
        .current_dir(repo)
        .env("USAGI_HOME", home)
        .env("PATH", fixture_path)
        .env("USAGI_PTY_SENTINEL", "must-not-leak")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE");
    if let Some(shell) = shell {
        command.env("SHELL", shell);
    }
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("root daemon starts");
    Daemon { child }
}

fn client(data_dir: &Path) -> IpcClient<std::os::unix::net::UnixStream> {
    let deadline = Instant::now() + DAEMON_READINESS_TIMEOUT;
    loop {
        if let Ok(stream) = connect_current(data_dir) {
            return IpcClient::connect(
                stream,
                "agent-ipc-e2e".into(),
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

fn available_scope(client: &mut impl DaemonClient) -> (WorkspaceId, SessionId, WorktreeId) {
    let reply = client
        .request(DaemonRequest::Session {
            action: SessionAction::Create,
            operation_id: OperationId::new().to_string(),
            payload: serde_json::json!({"name": "agent-e2e"}),
        })
        .expect("session fixture is created through root IPC");
    let body = match reply {
        DaemonReply::Accepted { body, .. } | DaemonReply::Ok(body) => body,
    };
    let workspace = serde_json::from_value(body["workspace_id"].clone()).unwrap();
    let sessions = body["sessions"].as_array().expect("session snapshot array");
    let session = sessions
        .iter()
        .find(|session| session["name"] == "agent-e2e")
        .expect("created session is present");
    (
        workspace,
        serde_json::from_value(session["session_id"].clone()).unwrap(),
        serde_json::from_value(session["worktree_id"].clone()).unwrap(),
    )
}

fn launch(
    client: &mut impl DaemonClient,
    workspace: WorkspaceId,
    session: SessionId,
    profile: Option<&str>,
) -> (String, TerminalRef) {
    let operation = OperationId::new().to_string();
    let reply = client
        .request(DaemonRequest::Agent {
            operation_id: operation.clone(),
            intent: AgentLaunchIntent {
                workspace,
                session: Some(session),
                profile: profile.map(|value| AgentProfileId::new(value).unwrap()),
            },
        })
        .expect("fixture Codex is admitted");
    let DaemonReply::Accepted {
        operation_id: accepted,
        body,
        ..
    } = reply
    else {
        panic!("launch must be accepted before its PTY exits: {reply:?}");
    };
    assert_eq!(
        accepted, operation,
        "admission preserves the client operation ID"
    );
    (
        operation,
        serde_json::from_value(body["terminal"].clone()).unwrap(),
    )
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
        panic!("terminal request must not be an operation admission");
    };
    body["subscription"].as_u64().expect("subscription id")
}

fn wait_for_agent_completion(
    client: &mut impl DaemonClient,
    operation: &str,
    workspace: WorkspaceId,
    session: SessionId,
    profile: Option<&str>,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match client.request(DaemonRequest::Agent {
            operation_id: operation.to_owned(),
            intent: AgentLaunchIntent {
                workspace,
                session: Some(session),
                profile: profile.map(|value| AgentProfileId::new(value).unwrap()),
            },
        }) {
            Ok(DaemonReply::Ok(body)) if body["completed"] == true => return body,
            Ok(DaemonReply::Accepted { .. }) => {}
            other => panic!("unexpected final replay: {other:?}"),
        }
        assert!(Instant::now() < deadline, "fixture Agent did not exit");
        thread::sleep(Duration::from_millis(20));
    }
}

fn resume(client: &mut impl DaemonClient, session_name: &str) -> (String, TerminalRef) {
    let operation = OperationId::new().to_string();
    let reply = client
        .request(DaemonRequest::Session {
            action: SessionAction::ResumeAgent,
            operation_id: operation.clone(),
            payload: serde_json::json!({"name": session_name}),
        })
        .expect("captured Codex conversation resumes through root IPC");
    let DaemonReply::Accepted { body, .. } = reply else {
        panic!("resume must be admitted as a daemon operation")
    };
    (
        operation,
        serde_json::from_value(body["terminal"].clone()).unwrap(),
    )
}

fn wait_for_resume_completion(client: &mut impl DaemonClient, operation: &str, session_name: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let reply = client
            .request(DaemonRequest::Session {
                action: SessionAction::ResumeAgent,
                operation_id: operation.to_owned(),
                payload: serde_json::json!({"name": session_name}),
            })
            .expect("resume replay is available");
        let body = match reply {
            DaemonReply::Accepted { body, .. } | DaemonReply::Ok(body) => body,
        };
        if body["completed"] == true {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "resumed fixture Agent did not exit"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn safe_readiness_error(error: ClientError) {
    let ClientError::Protocol(error) = error else {
        panic!("readiness failure must be a daemon protocol error");
    };
    assert_eq!(error.code, ErrorCode::Unavailable);
    assert!(error.message.contains("install it and sign in"));
    for private in ["PATH", "codex login status", "credential", "token", "argv"] {
        assert!(
            !error.message.contains(private),
            "leaked {private}: {error:?}"
        );
    }
}

#[test]
fn root_ipc_fixture_codex_survives_disconnect_and_replays_final() {
    let _serial = DAEMON_START_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let repo = fixture_repo();
    let home = short_dir("usagi-");
    let bin = home.path().join("bin");
    let count = home.path().join("spawn-count");
    write_codex(&bin, &count, 0);
    let _daemon = start_daemon(repo.path(), home.path(), &bin, None);
    let data_dir = channel_data_dir(home.path());
    let mut first = client(&data_dir);
    let (workspace, session, _) = available_scope(&mut first);

    // Omitted profile and explicit `codex` both resolve through the root's
    // Codex default/registry path.  The omitted launch drives the full stream.
    let (operation, terminal) = launch(&mut first, workspace, session, None);
    thread::sleep(Duration::from_millis(100));
    let subscription = attach(&mut first, &terminal);
    first
        .request(DaemonRequest::Terminal {
            action: TerminalAction::Detach,
            payload: serde_json::to_value(TerminalRequest::Detach {
                terminal: terminal.clone(),
                subscription,
            })
            .unwrap(),
        })
        .unwrap();
    drop(first); // connection teardown must only drop this subscription.

    let mut reattached = client(&data_dir);
    let subscription = attach(&mut reattached, &terminal);
    reattached
        .request(DaemonRequest::Terminal {
            action: TerminalAction::Input,
            payload: serde_json::to_value(TerminalRequest::Input {
                terminal: terminal.clone(),
                subscription,
                input_seq: 0,
                bytes: b"go\n".to_vec(),
            })
            .unwrap(),
        })
        .unwrap();

    let final_body =
        wait_for_agent_completion(&mut reattached, &operation, workspace, session, None);
    let replay: TerminalRef = serde_json::from_value(final_body["terminal"].clone()).unwrap();
    assert_eq!(replay, terminal);
    let snapshot = reattached
        .request(DaemonRequest::Terminal {
            action: TerminalAction::Resync,
            payload: serde_json::to_value(TerminalRequest::Resync {
                terminal: terminal.clone(),
            })
            .unwrap(),
        })
        .unwrap();
    let DaemonReply::Ok(snapshot) = snapshot else {
        unreachable!()
    };
    assert_eq!(snapshot["exited"], 0);
    assert!(snapshot["replay"].as_array().unwrap().len() >= b"ready\r\ninput:go\r\n".len());
    let durable = fs::read_to_string(data_dir.join("daemon/agents.json")).unwrap();
    assert!(durable.contains("provider_structured"));

    let (resume_operation, resumed_terminal) = resume(&mut reattached, "agent-e2e");
    assert_ne!(terminal, resumed_terminal);
    let resumed_subscription = attach(&mut reattached, &resumed_terminal);
    reattached
        .request(DaemonRequest::Terminal {
            action: TerminalAction::Input,
            payload: serde_json::to_value(TerminalRequest::Input {
                terminal: resumed_terminal,
                subscription: resumed_subscription,
                input_seq: 0,
                bytes: b"done\n".to_vec(),
            })
            .unwrap(),
        })
        .unwrap();
    assert_ne!(operation, resume_operation);
    wait_for_resume_completion(&mut reattached, &resume_operation, "agent-e2e");
    assert_eq!(fs::read_to_string(count).unwrap().lines().count(), 2);
}

#[test]
fn root_ipc_missing_or_not_authenticated_codex_is_safe_and_redacted() {
    let _serial = DAEMON_START_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for ready_status in [None, Some(1)] {
        let repo = fixture_repo();
        let home = short_dir("usagi-");
        let bin = home.path().join("bin");
        let count = home.path().join("spawn-count");
        fs::create_dir(&bin).unwrap();
        if let Some(status) = ready_status {
            write_codex(&bin, &count, status);
        }
        let _daemon = start_daemon(repo.path(), home.path(), &bin, None);
        let data_dir = channel_data_dir(home.path());
        let mut client = client(&data_dir);
        let (workspace, session, _) = available_scope(&mut client);
        let operation = OperationId::new().to_string();
        let request = || DaemonRequest::Agent {
            operation_id: operation.clone(),
            intent: AgentLaunchIntent {
                workspace,
                session: Some(session),
                profile: Some(AgentProfileId::new("codex").unwrap()),
            },
        };
        safe_readiness_error(client.request(request()).unwrap_err());
        safe_readiness_error(client.request(request()).unwrap_err());
        assert!(!count.exists(), "readiness failure must not spawn the PTY");
    }
}

#[test]
fn root_ipc_fixture_login_shell_is_fenced_and_replays_exit() {
    let _serial = DAEMON_START_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let repo = fixture_repo();
    let home = short_dir("usagi-");
    let bin = home.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let count = home.path().join("shell-spawn-count");
    let shell = bin.join("fixture-shell");
    write_shell(&shell, &count);
    let _daemon = start_daemon(repo.path(), home.path(), &bin, Some(&shell));
    let data_dir = channel_data_dir(home.path());
    let mut client = client(&data_dir);
    let (workspace, session, worktree) = available_scope(&mut client);

    let mut launch = |scope: TerminalLaunchScope, profile: &str| {
        client.request(DaemonRequest::Terminal {
            action: TerminalAction::Launch,
            payload: serde_json::to_value(TerminalRequest::Launch {
                intent: TerminalLaunchIntent {
                    request: TerminalLaunchRequest {
                        profile_id: TerminalProfileId::new(profile).unwrap(),
                        scope,
                    },
                    geometry: TerminalGeometry { cols: 80, rows: 24 },
                },
            })
            .unwrap(),
        })
    };
    let scope = TerminalLaunchScope {
        workspace_id: workspace,
        session_id: Some(session),
        worktree_id: worktree,
    };

    let unknown = launch(scope.clone(), "untrusted-profile").unwrap_err();
    assert_eq!(unknown.code(), ErrorCode::InvalidArgument);
    assert!(!count.exists(), "unknown profile must not spawn a shell");

    let stale = launch(
        TerminalLaunchScope {
            worktree_id: WorktreeId::new(),
            ..scope.clone()
        },
        "login-shell",
    )
    .unwrap_err();
    assert_eq!(stale.code(), ErrorCode::InvalidArgument);
    assert!(!count.exists(), "stale scope must not spawn a shell");

    let DaemonReply::Ok(launched) = launch(scope, "login-shell").unwrap() else {
        panic!("generic terminal launch is synchronous");
    };
    let terminal: TerminalRef = serde_json::from_value(launched["terminal"].clone()).unwrap();
    assert_eq!(terminal.workspace_id, workspace);
    assert_eq!(terminal.session_id, Some(session));
    assert_eq!(terminal.worktree_id, worktree);
    let subscription = attach(&mut client, &terminal);
    client
        .request(DaemonRequest::Terminal {
            action: TerminalAction::Input,
            payload: serde_json::to_value(TerminalRequest::Input {
                terminal: terminal.clone(),
                subscription,
                input_seq: 0,
                bytes: b"go\n".to_vec(),
            })
            .unwrap(),
        })
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let DaemonReply::Ok(snapshot) = client
            .request(DaemonRequest::Terminal {
                action: TerminalAction::Resync,
                payload: serde_json::to_value(TerminalRequest::Resync {
                    terminal: terminal.clone(),
                })
                .unwrap(),
            })
            .unwrap()
        else {
            unreachable!()
        };
        if snapshot["exited"] == 0 {
            let replay: Vec<u8> = serde_json::from_value(snapshot["replay"].clone()).unwrap();
            assert!(
                replay
                    .windows(b"shell-ready\r\n".len())
                    .any(|v| v == b"shell-ready\r\n")
            );
            assert!(
                replay
                    .windows(b"shell-input:go\r\n".len())
                    .any(|v| v == b"shell-input:go\r\n")
            );
            break;
        }
        assert!(Instant::now() < deadline, "fixture shell did not exit");
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(fs::read_to_string(count).unwrap().lines().count(), 1);
}

//! Root daemon Agent IPC regression with a fixture Codex executable.
//!
//! This deliberately starts the shipping composition root and talks only over
//! its Unix socket.  The fixture is placed on PATH, so neither a real Codex
//! installation nor credentials are needed.

#![cfg(unix)]

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use usagi_core::domain::agent::AgentProfileId;
use usagi_core::domain::id::{OperationId, SessionId, TerminalRef, WorkspaceId, WorktreeId};
use usagi_core::domain::session_lifecycle::AgentPhase;
use usagi_core::domain::terminal_launch::{
    TerminalLaunchRequest, TerminalLaunchScope, TerminalProfileId,
};
use usagi_core::infrastructure::ipc::ErrorCode;
use usagi_core::usecase::client::{
    AgentLaunchIntent, ClientError, ClientPolicy, DaemonClient, DaemonReply, DaemonRequest,
    IpcClient, McpCallerContext, SessionAction, TerminalAction, TerminalGeometry,
    TerminalLaunchIntent, TerminalRequest,
};
use usagi_daemon::infrastructure::unix_transport::connect_current;

/// daemon の起動はすべて共有 fixture 経由にする（cwd の fixture 固定と exact reap）。
#[path = "support/daemon.rs"]
mod daemon_fixture;

use daemon_fixture::{Channel, usagi_command};

fn shipping_build_identity() -> usagi_core::infrastructure::ipc::BuildIdentity {
    usagi_core::infrastructure::ipc::build_identity(
        env!("CARGO_PKG_VERSION"),
        env!("USAGI_BUILD_COMMIT"),
        env!("USAGI_BUILD_TARGET"),
        env!("USAGI_BUILD_PROFILE"),
        env!("USAGI_BUILD_SOURCE_ID"),
    )
}

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
    daemon_fixture::short_dir(prefix)
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
    spawn_daemon(repo, home, path, shell)
}

/// Start a daemon against an existing home, as a cold restart does. The data
/// directory is already published, so it is not re-created here.
fn spawn_daemon(repo: &Path, home: &Path, path: &Path, shell: Option<&Path>) -> Daemon {
    let fixture_path = format!("{}:/usr/bin:/bin", path.display());
    let mut command = usagi_command(
        home,
        Channel::Local,
        repo,
        &["daemon".as_ref(), "serve".as_ref()],
    );
    command
        .env("PATH", fixture_path)
        .env("USAGI_PTY_SENTINEL", "must-not-leak")
        // Claude は OS sandbox launcher 経由で起動するため、backend を持たない CI でも live
        // 配線を通すテスト専用 seam を有効にする（debug ビルド限定）。
        .env(
            usagi_core::usecase::claude_sandbox::PASSTHROUGH_ENVIRONMENT_VARIABLE,
            "1",
        );
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

/// This test process's client incarnation, shared by every connection it opens.
fn client_incarnation() -> &'static str {
    static INCARNATION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    INCARNATION.get_or_init(|| usagi_core::domain::id::ClientId::new().as_str())
}

fn client(data_dir: &Path) -> IpcClient<std::os::unix::net::UnixStream> {
    let deadline = Instant::now() + DAEMON_READINESS_TIMEOUT;
    let daemon_dir = data_dir.join("daemon");
    loop {
        // `connect_current` creates a missing endpoint directory for general
        // callers. This fixture starts the daemon concurrently, so wait for
        // the owner to create and privatise it instead of racing that setup.
        if daemon_dir.exists()
            && let Ok(stream) = connect_current(data_dir)
        {
            return IpcClient::connect(
                stream,
                // A canonical, process-stable client incarnation, exactly as the
                // composition root declares. The daemon keys durable per-client
                // state (the terminal input operation ledger) on it, so every
                // connection this fixture opens must present the same value.
                client_incarnation().to_owned(),
                OperationId::new().to_string(),
                ClientPolicy::cli(),
                shipping_build_identity(),
                daemon_fixture::client_workspace(data_dir),
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

fn launch_intent(
    workspace: WorkspaceId,
    session: SessionId,
    profile: Option<&str>,
) -> AgentLaunchIntent {
    AgentLaunchIntent {
        workspace,
        session: Some(session),
        profile: profile.map(|value| AgentProfileId::new(value).unwrap()),
    }
}

/// The digest a client computes for its own request, so an answer that means
/// another intent cannot be correlated to it (#522).
fn expected_digest(intent: &AgentLaunchIntent) -> String {
    usagi_core::infrastructure::ipc::agent_operation_digest(
        &usagi_core::usecase::client::agent_launch_semantic_key(intent),
    )
}

/// Assert that one Agent answer states the operation it belongs to and the digest
/// of the intent it was admitted for.
fn assert_agent_identity(body: &serde_json::Value, operation: &str, intent: &AgentLaunchIntent) {
    assert_eq!(
        body["operation_id"], *operation,
        "every Agent answer names its own operation"
    );
    assert_eq!(
        body["semantic_digest"],
        serde_json::Value::String(expected_digest(intent)),
        "every Agent answer carries the digest of the intent it was admitted for"
    );
}

fn launch(
    client: &mut impl DaemonClient,
    workspace: WorkspaceId,
    session: SessionId,
    profile: Option<&str>,
) -> (String, TerminalRef) {
    let operation = OperationId::new().to_string();
    let intent = launch_intent(workspace, session, profile);
    let reply = client
        .request(DaemonRequest::Agent {
            operation_id: operation.clone(),
            intent: intent.clone(),
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
    assert_agent_identity(&body, &operation, &intent);
    assert_eq!(
        body["completed"], false,
        "an admission is not offered as the durable final"
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

/// The screen a revision 2 snapshot restores to, rendered as retained rows.
///
/// A negotiated checkpoint connection never receives a raw tail, so this asserts
/// the frame's shape (one payload, complete at `output_offset`) before restoring
/// the daemon's authoritative screen through the shared parser.
fn restored_screen(snapshot: &serde_json::Value) -> Vec<String> {
    use usagi_core::usecase::vt_screen::{ScreenCheckpoint, VtScreen};

    assert!(
        snapshot["replay"].is_null(),
        "a revision 2 frame carries only the checkpoint"
    );
    assert_eq!(
        snapshot["base_offset"], snapshot["output_offset"],
        "a checkpoint is complete at output_offset"
    );
    let checkpoint: ScreenCheckpoint = serde_json::from_value(snapshot["screen"].clone())
        .expect("revision 2 snapshot carries a screen checkpoint");
    VtScreen::from_checkpoint(&checkpoint)
        .expect("the daemon checkpoint restores")
        .cells_with_scrollback()
}

/// Whether any restored row contains `text`.
fn screen_contains(rows: &[String], text: &str) -> bool {
    rows.iter().any(|row| row.contains(text))
}

/// Poll the durable final of one Agent launch, then read it once more.
///
/// `ResponseOutcome::Ok` carries no envelope operation identity, so the final and
/// the cached replay a reconnecting client reads must both state the operation and
/// the semantic digest in their body — and state them identically (#522).
fn wait_for_agent_completion(
    client: &mut impl DaemonClient,
    operation: &str,
    workspace: WorkspaceId,
    session: SessionId,
    profile: Option<&str>,
) -> serde_json::Value {
    let intent = launch_intent(workspace, session, profile);
    let request = || DaemonRequest::Agent {
        operation_id: operation.to_owned(),
        intent: intent.clone(),
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let body = loop {
        match client.request(request()) {
            Ok(DaemonReply::Ok(body)) if body["completed"] == true => break body,
            Ok(DaemonReply::Accepted { body, .. }) => {
                assert_agent_identity(&body, operation, &intent);
            }
            other => panic!("unexpected final replay: {other:?}"),
        }
        assert!(Instant::now() < deadline, "fixture Agent did not exit");
        thread::sleep(Duration::from_millis(20));
    };
    assert_agent_identity(&body, operation, &intent);
    let Ok(DaemonReply::Ok(replayed)) = client.request(request()) else {
        panic!("a completed operation replays its durable final");
    };
    assert_eq!(
        replayed, body,
        "the cached replay is the same identity-bearing final"
    );
    body
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
                input_operation: None,
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
    let rows = restored_screen(&snapshot);
    assert!(screen_contains(&rows, "ready"), "{rows:?}");
    assert!(screen_contains(&rows, "input:go"), "{rows:?}");
    let durable = serde_json::to_string(&durable_records(&data_dir)).unwrap();
    assert!(durable.contains("provider_structured"), "{durable}");

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
                input_operation: None,
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

/// An agent phase report is routed to the Agent owner over the real socket, and
/// the credential is the only thing that can bind it to a runtime. A forged or
/// malformed report is refused instead of silently answering `ok`.
#[test]
fn root_ipc_agent_phase_report_without_a_live_credential_fails_closed() {
    let _serial = DAEMON_START_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let repo = fixture_repo();
    let home = short_dir("usagi-");
    let bin = home.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let _daemon = start_daemon(repo.path(), home.path(), &bin, None);
    let mut client = client(&channel_data_dir(home.path()));

    let forged = client
        .request(DaemonRequest::AgentPhaseReport {
            phase: AgentPhase::Waiting,
            caller_context: McpCallerContext {
                credential: "forged-credential".into(),
            },
        })
        .unwrap_err();
    assert_eq!(forged.code(), ErrorCode::OwnershipUnknown);
    assert!(!forged.is_transport_failure(), "{forged}");
    assert!(!forged.to_string().contains("forged-credential"));

    // An empty credential is refused before the Agent owner is consulted.
    let empty = client
        .request(DaemonRequest::AgentPhaseReport {
            phase: AgentPhase::Ready,
            caller_context: McpCallerContext {
                credential: String::new(),
            },
        })
        .unwrap_err();
    assert_eq!(empty.code(), ErrorCode::InvalidArgument);
}

#[test]
#[allow(clippy::too_many_lines)] // One generic-terminal product flow, asserted end to end.
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
    let mut first = client(&data_dir);
    let (workspace, session, worktree) = available_scope(&mut first);

    let mut launch = |scope: TerminalLaunchScope, profile: &str| {
        first.request(DaemonRequest::Terminal {
            action: TerminalAction::Launch,
            payload: serde_json::to_value(TerminalRequest::Launch {
                intent: TerminalLaunchIntent {
                    request: TerminalLaunchRequest {
                        profile_id: TerminalProfileId::new(profile).unwrap(),
                        scope,
                    },
                    geometry: TerminalGeometry { cols: 80, rows: 24 },
                    launch_operation: None,
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
    let subscription = attach(&mut first, &terminal);
    // #519: carry this input's durable operation identity, then throw the
    // connection away as a lost acknowledgement would.
    let input_operation = OperationId::new();
    first
        .request(DaemonRequest::Terminal {
            action: TerminalAction::Input,
            payload: serde_json::to_value(TerminalRequest::Input {
                terminal: terminal.clone(),
                subscription,
                input_seq: 0,
                input_operation: Some(input_operation),
                bytes: b"go\n".to_vec(),
            })
            .unwrap(),
        })
        .unwrap();
    drop(first);

    // A fresh connection, a fresh subscription, and an epoch-local sequence back
    // at zero: only the operation identity still ties this to the earlier write.
    let mut reconnected = client(&data_dir);
    let DaemonReply::Ok(resolved) = reconnected
        .request(DaemonRequest::Terminal {
            action: TerminalAction::InputOutcome,
            payload: serde_json::to_value(TerminalRequest::InputOutcome {
                terminal: terminal.clone(),
                input_operation,
            })
            .unwrap(),
        })
        .unwrap()
    else {
        panic!("resolving an input operation is a synchronous read");
    };
    assert_eq!(resolved["outcome"], "final");
    assert_eq!(resolved["ack"], "Written");
    // An operation this daemon never recorded is a typed unknown, never a
    // fabricated success.
    let DaemonReply::Ok(unknown) = reconnected
        .request(DaemonRequest::Terminal {
            action: TerminalAction::InputOutcome,
            payload: serde_json::to_value(TerminalRequest::InputOutcome {
                terminal: terminal.clone(),
                input_operation: OperationId::new(),
            })
            .unwrap(),
        })
        .unwrap()
    else {
        panic!("resolving an input operation is a synchronous read");
    };
    assert_eq!(unknown["outcome"], "unknown");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let DaemonReply::Ok(snapshot) = reconnected
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
            let rows = restored_screen(&snapshot);
            assert!(screen_contains(&rows, "shell-ready"), "{rows:?}");
            // Exactly one echo: the operation was replayed as an answer, never
            // as a second write to the PTY.
            assert_eq!(
                rows.iter()
                    .filter(|row| row.contains("shell-input:go"))
                    .count(),
                1,
                "{rows:?}"
            );
            break;
        }
        assert!(Instant::now() < deadline, "fixture shell did not exit");
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(fs::read_to_string(count).unwrap().lines().count(), 1);
}

/// Wait until the fixture provider has recorded `expected` spawns. The fixture
/// appends one line per child, so this is the observable spawn count without
/// depending on a fixed sleep.
fn wait_for_spawns(count: &Path, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let observed = fs::read_to_string(count)
            .map(|body| body.lines().count())
            .unwrap_or_default();
        assert!(
            observed <= expected,
            "the fixture spawned more children than the {expected} expected"
        );
        if observed == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the fixture provider did not reach {expected} spawns"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// #510 product E2E: after a cold restart every interrupted conversation becomes
/// its own tab, and only an explicit per-tab resume starts a provider.
///
/// The daemon is `SIGKILL`ed (not stopped) so the old PTYs are genuinely gone,
/// then a fresh daemon is started against the same home. Fresh start, inventory,
/// and the TUI projection must invoke zero provider resumes; the one explicit
/// action must produce exactly one operation, one child spawn, and one new
/// `TerminalRef` for exactly the selected lineage.
#[test]
#[allow(clippy::too_many_lines)] // One cold-restart product flow, asserted end to end.
fn root_ipc_cold_restart_projects_interrupted_history_and_resumes_one_exact_tab() {
    use std::collections::BTreeSet;
    use usagi_core::domain::agent::{
        AgentInventory, AgentResumeRelation, AgentRuntimeInventoryState,
    };
    use usagi_tui::usecase::application::interrupted_tab::{
        accept_replacement, project, resume_command,
    };

    let _serial = DAEMON_START_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let repo = fixture_repo();
    let home = short_dir("usagi-");
    let bin = home.path().join("bin");
    let count = home.path().join("spawn-count");
    write_codex(&bin, &count, 0);
    let data_dir = channel_data_dir(home.path());

    // One managed-session Agent and one workspace-root Agent, so the restart
    // leaves two distinct conversation lineages in two distinct scopes.
    let daemon = start_daemon(repo.path(), home.path(), &bin, None);
    let mut first = client(&data_dir);
    let (workspace, session, _) = available_scope(&mut first);
    let (session_operation, session_terminal) = launch(&mut first, workspace, session, None);
    let root_operation = OperationId::new().to_string();
    let root_intent = AgentLaunchIntent {
        workspace,
        session: None,
        profile: None,
    };
    let root_reply = first
        .request(DaemonRequest::Agent {
            operation_id: root_operation.clone(),
            intent: root_intent.clone(),
        })
        .expect("workspace-root fixture Codex is admitted");
    let DaemonReply::Accepted { body, .. } = root_reply else {
        panic!("root launch must be admitted as an operation");
    };
    // Two concurrent scopes: each admission names its own operation and its own
    // intent, so neither client can correlate the other's answer (#522).
    assert_ne!(root_operation, session_operation);
    assert_agent_identity(&body, &root_operation, &root_intent);
    assert_ne!(
        body["semantic_digest"],
        serde_json::Value::String(expected_digest(&launch_intent(workspace, session, None)))
    );
    let root_terminal: TerminalRef = serde_json::from_value(body["terminal"].clone()).unwrap();
    assert_eq!(root_terminal.session_id, None);
    // Both fixture children are running before the cold failure.
    wait_for_spawns(&count, 2);
    let spawns_before_restart = 2;

    // A cold failure: SIGKILL, not a `daemon stop` that retires live resources.
    drop(first);
    drop(daemon);
    let _restarted = spawn_daemon(repo.path(), home.path(), &bin, None);
    let mut client = client(&data_dir);

    let inventory = |client: &mut IpcClient<std::os::unix::net::UnixStream>| -> AgentInventory {
        let reply = client
            .request(DaemonRequest::AgentInventory { workspace })
            .expect("Agent inventory is available after a cold restart");
        let body = match reply {
            DaemonReply::Accepted { body, .. } | DaemonReply::Ok(body) => body,
        };
        serde_json::from_value(body).expect("inventory decodes into the shared projection")
    };
    let observed = inventory(&mut client);
    assert_eq!(observed.workspace_id, workspace);
    // No old PTY was restored as live.
    assert!(
        observed
            .runtimes
            .iter()
            .all(|item| item.state != AgentRuntimeInventoryState::Live),
        "{observed:?}"
    );

    // The shipping TUI reducer projects the inventory. Nothing here resumes.
    let allowed = BTreeSet::from([session]);
    let projection = project(
        &observed,
        workspace,
        &allowed,
        &[],
        &BTreeSet::new(),
        &BTreeSet::new(),
    );
    assert_eq!(projection.tabs.len(), 2, "{projection:?}");
    // Root and managed-session histories are separate, stable tabs.
    assert!(
        projection
            .tabs
            .iter()
            .any(|tab| tab.session_id.is_none() && tab.last_terminal.fences(&root_terminal))
    );
    assert!(
        projection
            .tabs
            .iter()
            .any(|tab| tab.session_id == Some(session)
                && tab.last_terminal.fences(&session_terminal))
    );
    // A repeated observation converges to the same tabs instead of duplicating.
    let again = project(
        &inventory(&mut client),
        workspace,
        &allowed,
        &[],
        &BTreeSet::new(),
        &BTreeSet::new(),
    );
    assert_eq!(again.tabs, projection.tabs);
    assert_eq!(
        fs::read_to_string(&count).unwrap().lines().count(),
        spawns_before_restart,
        "fresh start, inventory, and projection must not invoke a provider resume"
    );

    // One explicit resume of the root tab, carrying only the daemon's own opaque
    // target plus a fresh operation.
    let selected = projection
        .tabs
        .iter()
        .find(|tab| tab.session_id.is_none())
        .expect("the root history is projected");
    let other = projection
        .tabs
        .iter()
        .find(|tab| tab.session_id == Some(session))
        .expect("the managed-session history is projected");
    assert!(selected.resumable(), "{selected:?}");
    let command = resume_command(selected, None, OperationId::new()).expect("the tab is resumable");
    let request = DaemonRequest::ResumeAgent {
        operation_id: command.operation.to_string(),
        target: command.target.clone(),
    };
    let DaemonReply::Accepted { body, .. } = client
        .request(request)
        .expect("the exact target resumes through root IPC")
    else {
        panic!("an exact resume must be admitted as a daemon operation");
    };
    let replacement: TerminalRef = serde_json::from_value(body["terminal"].clone()).unwrap();
    let relation: AgentResumeRelation =
        serde_json::from_value(body["resume_relation"].clone()).expect("the relation is returned");
    let continuation = serde_json::from_value(body["continuation"].clone()).unwrap();
    // The TUI accepts the replacement only because every fence agrees.
    let accepted = accept_replacement(
        selected,
        command.operation,
        command.operation,
        continuation,
        Some(&relation),
        &replacement,
    )
    .expect("the daemon answer satisfies every resume fence");
    assert_eq!(accepted.continuation, selected.continuation);
    assert!(!replacement.fences(&root_terminal));
    assert_eq!(replacement.session_id, None);

    // A replayed click resolves to the same operation without a second spawn.
    let replayed = client
        .request(DaemonRequest::ResumeAgent {
            operation_id: command.operation.to_string(),
            target: command.target.clone(),
        })
        .expect("a replayed exact resume is idempotent");
    let replayed_terminal: TerminalRef = match replayed {
        DaemonReply::Accepted { body, .. } | DaemonReply::Ok(body) => {
            serde_json::from_value(body["terminal"].clone()).unwrap()
        }
    };
    assert_eq!(replayed_terminal, replacement);
    // One explicit resume spawns exactly one replacement child, and the replay
    // never adds a second one.
    wait_for_spawns(&count, spawns_before_restart + 1);

    // The other lineage is untouched: still interrupted, still resumable, and
    // never replaced by this operation.
    let after = project(
        &inventory(&mut client),
        workspace,
        &allowed,
        &[],
        &BTreeSet::new(),
        &BTreeSet::new(),
    );
    let untouched = after
        .tabs
        .iter()
        .find(|tab| tab.continuation == other.continuation)
        .expect("the unselected history keeps its own tab");
    assert_eq!(untouched.last_terminal, other.last_terminal);
    assert!(untouched.resumable());
    // The resumed lineage converged onto its live replacement, so it no longer
    // projects an interrupted tab.
    assert!(
        after
            .tabs
            .iter()
            .all(|tab| tab.continuation != selected.continuation),
        "{after:?}"
    );
}

/// A planned `daemon stop` / `daemon restart` must not destroy a live PTY, and
/// an explicit `--force` must still be able to.
///
/// This drives the shipping binary end to end: a real daemon process owning a
/// real generic-terminal child, and the same `usagi daemon …` verbs an operator
/// runs. The refusal has to be observable there, not only in the usecase.
#[test]
fn root_planned_stop_and_restart_refuse_while_a_terminal_is_live() {
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

    let DaemonReply::Ok(launched) = client
        .request(DaemonRequest::Terminal {
            action: TerminalAction::Launch,
            payload: serde_json::to_value(TerminalRequest::Launch {
                intent: TerminalLaunchIntent {
                    request: TerminalLaunchRequest {
                        profile_id: TerminalProfileId::new("login-shell").unwrap(),
                        scope: TerminalLaunchScope {
                            workspace_id: workspace,
                            session_id: Some(session),
                            worktree_id: worktree,
                        },
                    },
                    geometry: TerminalGeometry { cols: 80, rows: 24 },
                    launch_operation: None,
                },
            })
            .unwrap(),
        })
        .expect("the fixture login shell launches")
    else {
        panic!("generic terminal launch is synchronous");
    };
    let terminal: TerminalRef = serde_json::from_value(launched["terminal"].clone()).unwrap();
    let owner_pid = daemon_pid(&data_dir);
    let child_pid = live_terminal_pid(&data_dir);

    let lifecycle = |args: &[&str]| {
        let mut command = usagi_command(
            home.path(),
            Channel::Local,
            repo.path(),
            &args.iter().map(OsStr::new).collect::<Vec<_>>(),
        );
        command.output().expect("the shipping binary runs")
    };

    // Both planned verbs refuse, name what they saved, and name the missing
    // prerequisite that would otherwise have preserved it.
    for args in [&["daemon", "stop"][..], &["daemon", "restart"][..]] {
        let refused = lifecycle(args);
        let message = String::from_utf8_lossy(&refused.stderr).into_owned()
            + &String::from_utf8_lossy(&refused.stdout);
        assert!(!refused.status.success(), "{args:?} was not refused");
        assert!(
            message.contains("1 generic terminal(s)"),
            "{args:?}: {message}"
        );
        assert!(message.contains("--force"), "{args:?}: {message}");
    }
    assert!(
        message_free_of_effect(&data_dir, owner_pid, child_pid),
        "a refused transition changed the daemon or its child"
    );
    // The terminal is not merely alive: it is still the same owned runtime, so a
    // client can keep using the exact ref it already holds.
    assert_eq!(live_terminal_ref(&data_dir), terminal);

    // Giving the runtime up explicitly is what actually stops the daemon: the
    // owner clears its exact lifecycle record only after retiring its endpoint.
    let forced = lifecycle(&["daemon", "stop", "--force"]);
    assert!(
        forced.status.success(),
        "{}",
        String::from_utf8_lossy(&forced.stderr)
    );
    assert!(
        !data_dir.join("daemon/daemon.json").exists(),
        "the forced stop left the lifecycle record behind"
    );
    wait_for_dead(child_pid);
}

/// The exact ref of the single live generic terminal.
fn live_terminal_ref(data_dir: &Path) -> TerminalRef {
    live_terminal(data_dir).0
}

/// The OS pid of the single live generic terminal's child.
fn live_terminal_pid(data_dir: &Path) -> u64 {
    live_terminal(data_dir).1
}

fn live_terminal(data_dir: &Path) -> (TerminalRef, u64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let records = durable_records(data_dir);
        let found = records
            .iter()
            .find(|record| record["state"] == "running" && record["terminal"].is_object())
            .and_then(|record| {
                let terminal = serde_json::from_value(record["terminal"].clone()).ok()?;
                Some((terminal, record["process"]["pid"].as_u64()?))
            });
        if let Some(found) = found {
            return found;
        }
        assert!(
            Instant::now() < deadline,
            "no live generic terminal was persisted: {records:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// The production records every retained owner shard holds.
///
/// The durable runtime state is one document per owner generation now, and each
/// record travels as the opaque payload of its shard resource (#562). Reading them
/// back gives the tests the same record list the whole-snapshot stores used to
/// hold.
fn durable_records(data_dir: &Path) -> Vec<serde_json::Value> {
    let shards = data_dir.join("daemon").join("shards");
    let Ok(entries) = fs::read_dir(&shards) else {
        return Vec::new();
    };
    let mut records = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(document) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Some(resources) = document["resources"].as_array() else {
            continue;
        };
        records.extend(resources.iter().filter_map(|resource| {
            serde_json::from_str::<serde_json::Value>(resource["payload"].as_str()?).ok()
        }));
    }
    records
}

fn daemon_pid(data_dir: &Path) -> u64 {
    let record = fs::read_to_string(data_dir.join("daemon/daemon.json")).unwrap();
    serde_json::from_str::<serde_json::Value>(&record).unwrap()["pid"]
        .as_u64()
        .unwrap()
}

/// Whether the refused transition left the owner and its child exactly as they
/// were.
fn message_free_of_effect(data_dir: &Path, owner: u64, child: u64) -> bool {
    alive(owner) && alive(child) && daemon_pid(data_dir) == owner
}

fn alive(pid: u64) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only probes existence and permission.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Wait for a process this test never parented, so no zombie can be mistaken
/// for a survivor.
fn wait_for_dead(pid: u64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(pid) {
        assert!(Instant::now() < deadline, "process {pid} did not exit");
        thread::sleep(Duration::from_millis(20));
    }
}

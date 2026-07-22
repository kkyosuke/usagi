//! 配布バイナリの CLI 解析から TUI 起動画面までを通す結合テスト。

use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use usagi_core::infrastructure::ipc::{
    BuildIdentity, DaemonGeneration, Envelope, EnvelopeKind, ErrorCode, OperationId, ProtocolError,
    ResponseOutcome, read_json_frame, write_json_frame,
};
use usagi_daemon::infrastructure::unix_transport::{
    EndpointLocator, EndpointState, SecureUnixListener,
};

/// Daemon lifecycle tests spawn the same test binary as a background daemon.
/// Serialize those starts so parallel integration tests cannot race its process
/// discovery and readiness publication on a loaded CI runner.
static DAEMON_LIFECYCLE_LOCK: Mutex<()> = Mutex::new(());

fn short_home() -> tempfile::TempDir {
    // A Unix-domain socket includes the data directory, generation, and socket
    // name. Keep the integration fixture below the platform sockaddr limit.
    tempfile::Builder::new()
        .prefix("usagi-")
        .tempdir_in("/tmp")
        .expect("short daemon data directory")
}

fn channel_data_dir(home: &Path) -> PathBuf {
    usagi_core::infrastructure::paths::channel_data_dir(home)
}

fn run(args: &[&OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_usagi"))
        .args(args)
        .output()
        .expect("usagi バイナリを起動できる")
}

fn stop_daemon(home: &Path) {
    let output = Command::new(env!("CARGO_BIN_EXE_usagi"))
        .args([OsStr::new("daemon"), OsStr::new("stop")])
        .env("USAGI_HOME", home)
        .output()
        .expect("usagi daemon stop を起動できる");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_daemon_running(home: &Path) {
    let output = Command::new(env!("CARGO_BIN_EXE_usagi"))
        .args([OsStr::new("daemon"), OsStr::new("status")])
        .env("USAGI_HOME", home)
        .output()
        .expect("usagi daemon status を起動できる");
    assert!(output.status.success());
    assert!(stdout(&output).contains("daemon running"));
}

fn run_with_home(args: &[&OsStr], home: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_usagi"))
        .args(args)
        .env("USAGI_HOME", home)
        .output()
        .expect("usagi バイナリを起動できる")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[derive(Clone, Copy)]
enum FakeDaemonReply {
    CloseAfterRequest,
    Error(ErrorCode, &'static str, &'static str),
    Accepted,
    Ok,
}

fn spawn_fake_daemon(home: &Path, reply: FakeDaemonReply) -> thread::JoinHandle<()> {
    let data_dir = channel_data_dir(home);
    std::fs::create_dir_all(&data_dir).unwrap();
    let generation = DaemonGeneration(format!("fake-{}", std::process::id()));
    let listener = SecureUnixListener::bind(&data_dir, generation.clone()).unwrap();
    thread::spawn(move || {
        let mut stream = loop {
            match listener.accept() {
                Ok(stream) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("fake daemon accept failed: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        let mut writer = stream.try_clone().unwrap();
        let server = usagi_daemon::presentation::ipc::server_protocol(
            generation,
            "fake-connection".into(),
            BuildIdentity {
                version: env!("CARGO_PKG_VERSION").into(),
                commit: "unknown".into(),
                target: std::env::consts::ARCH.into(),
            },
        );
        let hello = usagi_daemon::presentation::ipc::handshake(&mut stream, &mut writer, &server)
            .unwrap()
            .unwrap();
        let request = read_json_frame::<Envelope>(&mut stream, 1_048_576)
            .unwrap()
            .unwrap();
        if matches!(reply, FakeDaemonReply::CloseAfterRequest) {
            return;
        }
        let EnvelopeKind::Request { request_id, .. } = request.kind else {
            panic!("fake daemon expected a request envelope");
        };
        let (outcome, body) = match reply {
            FakeDaemonReply::CloseAfterRequest => unreachable!(),
            FakeDaemonReply::Error(code, message, error_id) => {
                let mut error = ProtocolError::new(code, message);
                error.error_id = error_id.into();
                (ResponseOutcome::Error(error), serde_json::json!(null))
            }
            FakeDaemonReply::Accepted => (
                ResponseOutcome::Accepted {
                    operation_id: OperationId("fake-operation".into()),
                    operation_revision: 7,
                },
                serde_json::json!({"accepted": true}),
            ),
            FakeDaemonReply::Ok => (ResponseOutcome::Ok, serde_json::json!({"result": "done"})),
        };
        write_json_frame(
            &mut writer,
            &Envelope {
                protocol: hello.protocol,
                daemon_generation: hello.daemon_generation,
                kind: EnvelopeKind::Response {
                    request_id,
                    outcome,
                    body,
                },
            },
            1_048_576,
        )
        .unwrap();
    })
}

fn install_absent_daemon_endpoint(home: &Path) {
    let data_dir = channel_data_dir(home);
    let daemon = data_dir.join("daemon");
    let generation = DaemonGeneration("absent-generation".into());
    let generation_dir = daemon.join("generations").join(&generation.0);
    std::fs::create_dir_all(&generation_dir).unwrap();
    for directory in [&daemon, &daemon.join("generations"), &generation_dir] {
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let socket = generation_dir.join("sock");
    drop(UnixListener::bind(&socket).unwrap());
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    let locator = daemon.join("current.json");
    std::fs::write(
        &locator,
        serde_json::to_vec(&EndpointLocator {
            generation,
            endpoint: "generations/absent-generation/sock".into(),
            state: EndpointState::Active,
        })
        .unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(locator, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn run_mcp(home: &Path, cwd: &Path, requests: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_usagi"))
        .arg("mcp")
        .env("USAGI_HOME", home)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("usagi mcp を起動できる");
    let input = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":\"2025-06-18\"}}}}\n{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}}\n{requests}"
    );
    child
        .stdin
        .take()
        .expect("MCP stdin")
        .write_all(input.as_bytes())
        .expect("MCP requests を書き込める");
    child.wait_with_output().expect("MCP の終了を待てる")
}

fn mcp_texts(output: &Output) -> Vec<serde_json::Value> {
    stdout(output)
        .lines()
        .filter_map(|line| {
            let response: serde_json::Value = serde_json::from_str(line).unwrap();
            let content = response["result"]["content"].as_array()?;
            let text = content[0]["text"].as_str().unwrap();
            Some(serde_json::from_str(text).unwrap())
        })
        .collect()
}

fn mcp_responses(output: &Output) -> Vec<serde_json::Value> {
    stdout(output)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .filter(|response: &serde_json::Value| response["id"] != 0)
        .collect()
}

#[test]
fn welcome_entry_renders_the_welcome_screen() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // 引数なしと `hop` はどちらも welcome 画面を選ぶ。テストでは stdout が tty でないため、
    // 合成ルートは対話ループの代わりに welcome の 1 フレームを描いて返す。
    let home = short_home();
    for args in [&[][..], &[OsStr::new("hop")][..]] {
        let output = run_with_home(args, home.path());
        assert!(output.status.success(), "args={args:?}");
        let out = stdout(&output);
        assert!(out.contains("USAGI"), "args={args:?}");
        assert!(out.contains("Menu"), "args={args:?}");
        assert!(out.contains("q: quit"), "args={args:?}");
        assert!(output.stderr.is_empty(), "args={args:?}");
    }
    stop_daemon(home.path());
}

#[test]
fn daemon_status_reports_not_running_with_a_fresh_data_dir() {
    // `usagi daemon status` を実バイナリで走らせ、合成ルートが束ねる実ストア
    // （`FsRecordFile` を backing にした `DaemonRecordStore`）を通す。データディレクトリを
    // 空の一時パスへ向けるので、レコードは無く「daemon not running」を報告する。
    let home = short_home();
    let output = Command::new(env!("CARGO_BIN_EXE_usagi"))
        .args([OsStr::new("daemon"), OsStr::new("status")])
        .env("USAGI_HOME", home.path())
        .output()
        .expect("usagi バイナリを起動できる");
    assert!(output.status.success());
    assert!(stdout(&output).contains("daemon not running"));
}

#[test]
fn daemon_restart_initializes_a_private_endpoint_from_an_empty_data_dir() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let output = Command::new(env!("CARGO_BIN_EXE_usagi"))
        .args([OsStr::new("daemon"), OsStr::new("restart")])
        .env("USAGI_HOME", home.path())
        .output()
        .expect("usagi daemon restart を起動できる");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout(&output).contains("daemon restarted"));
    assert_daemon_running(home.path());
    stop_daemon(home.path());
}

#[test]
fn cli_daemon_request_autostarts_without_manual_daemon_start() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // This integration test owns the lifecycle contract.  Command payload
    // rendering is covered at the CLI/IPC boundary, and can legitimately
    // differ between accepted and immediately completed requests.
    let home = short_home();
    let output = run_with_home(
        &[
            OsStr::new("session"),
            OsStr::new("remove"),
            OsStr::new("missing"),
        ],
        home.path(),
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("session was not found"),
        "daemon request error: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_daemon_running(home.path());
    stop_daemon(home.path());
}

#[test]
fn cli_daemon_reply_contract_maps_stdout_stderr_and_exit_code() {
    struct Case {
        name: &'static str,
        reply: Option<FakeDaemonReply>,
        exit_code: i32,
        stdout: &'static str,
        stderr: &'static str,
    }

    let cases = [
        Case {
            name: "daemon absent",
            reply: None,
            exit_code: 1,
            stdout: "",
            stderr: "daemon unavailable [unavailable]: daemon endpoint is unavailable\n",
        },
        Case {
            name: "socket transport failure",
            reply: Some(FakeDaemonReply::CloseAfterRequest),
            exit_code: 1,
            stdout: "",
            stderr: "daemon request failed [unavailable]: daemon transport is unavailable\n",
        },
        Case {
            name: "protocol rejection",
            reply: Some(FakeDaemonReply::Error(
                ErrorCode::ProtocolMismatch,
                "protocol revision was rejected",
                "protocol-481",
            )),
            exit_code: 1,
            stdout: "",
            stderr: "daemon request failed [protocol_mismatch; error_id=protocol-481]: protocol revision was rejected\n",
        },
        Case {
            name: "stale application request",
            reply: Some(FakeDaemonReply::Error(
                ErrorCode::StaleTarget,
                "request target is stale",
                "stale-481",
            )),
            exit_code: 1,
            stdout: "",
            stderr: "daemon request failed [stale_target; error_id=stale-481]: request target is stale\n",
        },
        Case {
            name: "accepted",
            reply: Some(FakeDaemonReply::Accepted),
            exit_code: 0,
            stdout: "accepted operation fake-operation (revision 7)\n",
            stderr: "",
        },
        Case {
            name: "success",
            reply: Some(FakeDaemonReply::Ok),
            exit_code: 0,
            stdout: "{\"result\":\"done\"}\n",
            stderr: "",
        },
    ];

    for case in cases {
        let home = short_home();
        let server = if let Some(reply) = case.reply {
            Some(spawn_fake_daemon(home.path(), reply))
        } else {
            install_absent_daemon_endpoint(home.path());
            None
        };
        let output = run_with_home(
            &[
                OsStr::new("session"),
                OsStr::new("remove"),
                OsStr::new("fixture"),
            ],
            home.path(),
        );
        assert_eq!(output.status.code(), Some(case.exit_code), "{}", case.name);
        assert_eq!(stdout(&output), case.stdout, "{}", case.name);
        assert_eq!(stderr(&output), case.stderr, "{}", case.name);
        if let Some(server) = server {
            server.join().unwrap();
        }
    }
}

#[test]
fn mcp_autostarts_without_manual_daemon_start() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let mut child = Command::new(env!("CARGO_BIN_EXE_usagi"))
        .arg("mcp")
        .env("USAGI_HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("usagi mcp を起動できる");
    child
        .stdin
        .take()
        .expect("MCP stdin")
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\"}}\n")
        .expect("MCP initialize を書き込める");
    let output = child.wait_with_output().expect("MCP の終了を待てる");
    assert!(output.status.success());
    assert!(stdout(&output).contains("\"serverInfo\""));
    assert_daemon_running(home.path());
    stop_daemon(home.path());
}

#[test]
fn mcp_store_tools_round_trip_through_stdio_and_durable_files() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let workspace = tempfile::tempdir().unwrap();
    let session = workspace.path().join(".usagi/sessions/e2e");
    std::fs::create_dir_all(&session).unwrap();
    let requests = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_create\",\"arguments\":{\"title\":\"MCP durable issue\",\"priority\":\"high\",\"labels\":[\"mcp\"],\"body\":\"round trip\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_get\",\"arguments\":{\"number\":1}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_search\",\"arguments\":{\"query\":\"durable\",\"label\":\"mcp\",\"ready\":true}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"memory_save\",\"arguments\":{\"name\":\"MCP Fact\",\"title\":\"Durable fact\",\"type\":\"project\",\"body\":\"remember me\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"tools/call\",\"params\":{\"name\":\"memory_get\",\"arguments\":{\"name\":\"mcp-fact\"}}}\n",
    );
    let output = run_mcp(home.path(), &session, requests);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let values = mcp_texts(&output);
    assert_eq!(values[0]["number"], 1);
    assert_eq!(values[1]["title"], "MCP durable issue");
    assert_eq!(values[2][0]["ready"], true);
    assert_eq!(values[3]["name"], "mcp-fact");
    assert_eq!(values[4]["body"], "remember me");
    assert!(
        session
            .join(".usagi/issues/001-mcp-durable-issue.md")
            .is_file()
    );
    assert!(session.join(".usagi/memory/mcp-fact.md").is_file());
    stop_daemon(home.path());
}

#[test]
fn mcp_store_tools_cover_prompt_update_search_and_delete_lifecycles() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let workspace = tempfile::tempdir().unwrap();
    let session = workspace.path().join(".usagi/sessions/lifecycle");
    std::fs::create_dir_all(&session).unwrap();
    let requests = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_create\",\"arguments\":{\"title\":\"Lifecycle\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_to_prompt\",\"arguments\":{\"number\":1}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_update\",\"arguments\":{\"number\":1,\"status\":\"in-progress\",\"parent\":null}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"memory_save\",\"arguments\":{\"name\":\"life\",\"title\":\"Life\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"tools/call\",\"params\":{\"name\":\"memory_save\",\"arguments\":{\"name\":\"life\",\"body\":\"changed\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":6,\"method\":\"tools/call\",\"params\":{\"name\":\"memory_search\",\"arguments\":{\"query\":\"changed\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_delete\",\"arguments\":{\"number\":1}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":8,\"method\":\"tools/call\",\"params\":{\"name\":\"memory_delete\",\"arguments\":{\"name\":\"life\"}}}\n",
    );
    let output = run_mcp(home.path(), &session, requests);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let values = mcp_texts(&output);
    assert!(values[1]["prompt"].as_str().unwrap().contains("Lifecycle"));
    assert_eq!(values[2]["status"], "in-progress");
    assert_eq!(values[4]["body"], "changed");
    assert_eq!(values[5][0]["name"], "life");
    assert_eq!(values[6]["deleted"], true);
    assert_eq!(values[7]["deleted"], true);

    let missing_requests = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_update\",\"arguments\":{\"number\":1,\"status\":\"done\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"memory_get\",\"arguments\":{\"name\":\"life\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":{\"name\":\"memory_search\",\"arguments\":{\"type\":9}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"tools/call\",\"params\":{\"name\":\"memory_save\",\"arguments\":{\"name\":\"missing-title\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":13,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_create\",\"arguments\":{}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":14,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_to_prompt\",\"arguments\":{}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":15,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_search\",\"arguments\":{\"ready\":\"yes\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":16,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_update\",\"arguments\":{\"status\":\"done\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":17,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_delete\",\"arguments\":{}}}\n",
    );
    let missing = run_mcp(home.path(), &session, missing_requests);
    let missing_responses = mcp_responses(&missing);
    assert!(
        missing_responses[0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no issue")
    );
    assert_eq!(missing_responses[1]["result"]["content"][0]["text"], "null");
    assert_eq!(missing_responses[2]["error"]["code"], -32602);
    assert_eq!(missing_responses[3]["error"]["code"], -32603);
    for response in &missing_responses[4..] {
        assert_eq!(response["error"]["code"], -32602);
    }

    let broken_session = workspace.path().join(".usagi/sessions/broken");
    std::fs::create_dir_all(broken_session.join(".usagi")).unwrap();
    std::fs::write(broken_session.join(".usagi/issues"), "not a directory").unwrap();
    std::fs::write(broken_session.join(".usagi/memory"), "not a directory").unwrap();
    let broken_requests = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":18,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_create\",\"arguments\":{\"title\":\"Broken\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":19,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_get\",\"arguments\":{\"number\":1}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_to_prompt\",\"arguments\":{\"number\":1}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":21,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_search\",\"arguments\":{}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":22,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_update\",\"arguments\":{\"number\":1}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":23,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_delete\",\"arguments\":{\"number\":1}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":24,\"method\":\"tools/call\",\"params\":{\"name\":\"memory_save\",\"arguments\":{\"name\":\"fact\",\"title\":\"Fact\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":25,\"method\":\"tools/call\",\"params\":{\"name\":\"memory_search\",\"arguments\":{}}}\n",
    );
    let broken = run_mcp(home.path(), &broken_session, broken_requests);
    for response in mcp_responses(&broken) {
        assert_eq!(response["error"]["code"], -32603);
    }

    let root_requests = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":26,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_create\",\"arguments\":{\"title\":\"refused\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":27,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_update\",\"arguments\":{\"number\":1}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":28,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_delete\",\"arguments\":{\"number\":1}}}\n",
    );
    let refused = run_mcp(home.path(), workspace.path(), root_requests);
    for response in mcp_responses(&refused) {
        assert_eq!(response["error"]["code"], -32603);
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("workspace root")
        );
    }
    assert!(!workspace.path().join(".usagi/issues").exists());
    stop_daemon(home.path());
}

#[test]
fn config_entry_renders_the_config_screen() {
    // `usagi config` は Config 画面を選ぶ。stdout が tty でないため、合成ルートは対話ループの
    // 代わりに Config の 1 フレームを描いて返す。Config 自体は workspace registry を使わない
    // ため、registry が壊れていても起動できる。
    let home = short_home();
    std::fs::create_dir_all(channel_data_dir(home.path())).unwrap();
    std::fs::write(
        channel_data_dir(home.path()).join("workspaces.json"),
        "{ broken",
    )
    .unwrap();
    let output = run_with_home(&[OsStr::new("config")], home.path());
    assert!(output.status.success());
    let out = stdout(&output);
    assert!(out.contains("Config"));
    assert!(out.contains("Scope: Global"));
    assert!(out.contains("Theme") && out.contains("system"));
    assert!(out.contains("Esc: back"));
    assert!(output.stderr.is_empty());
    let status = Command::new(env!("CARGO_BIN_EXE_usagi"))
        .args([OsStr::new("daemon"), OsStr::new("status")])
        .env("USAGI_HOME", home.path())
        .output()
        .expect("usagi daemon status を起動できる");
    assert!(status.status.success());
    assert!(stdout(&status).contains("daemon not running"));
    stop_daemon(home.path());
}

#[test]
fn other_entries_route_to_their_banner_screens() {
    // 対話ループ未接続の画面（Doctor）は暫定バナー。
    let home = short_home();
    let output = run_with_home(&[OsStr::new("doctor")], home.path());
    assert!(output.status.success());
    assert!(stdout(&output).contains("doctor TUI"));
    assert!(output.stderr.is_empty());
    stop_daemon(home.path());
}

#[test]
fn open_registers_and_renders_an_explicit_or_current_workspace() {
    let home = short_home();
    let roots = tempfile::tempdir().unwrap();
    let explicit = roots.path().join("explicit-workspace");
    std::fs::create_dir(&explicit).unwrap();

    let output = run_with_home(&[OsStr::new("open"), explicit.as_os_str()], home.path());
    assert!(output.status.success());
    let out = stdout(&output);
    assert!(out.contains("explicit-workspace"));
    assert!(out.contains("main"));
    assert!(!out.contains("workspace TUI ("));

    // 非 tty でも open は registry へ登録し、続く hop の Recent に現れる。
    let registry =
        std::fs::read_to_string(channel_data_dir(home.path()).join("workspaces.json")).unwrap();
    assert!(registry.contains("explicit-workspace"));
    let output = run_with_home(&[OsStr::new("hop")], home.path());
    assert!(output.status.success());
    let out = stdout(&output);
    assert!(out.contains("Recent"));
    assert!(out.contains("explicit-workspace"));

    let current = roots.path().join("current-workspace");
    std::fs::create_dir(&current).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_usagi"))
        .arg("open")
        .current_dir(&current)
        .env("USAGI_HOME", home.path())
        .output()
        .expect("usagi バイナリを起動できる");
    assert!(output.status.success());
    let out = stdout(&output);
    assert!(out.contains("current-workspace"));
    assert!(out.contains("main"));
    stop_daemon(home.path());
}

#[test]
fn open_rejects_a_missing_or_non_directory_workspace_path() {
    let home = tempfile::tempdir().unwrap();
    let missing = home.path().join("missing-workspace");
    let file = home.path().join("not-a-directory");
    std::fs::write(&file, "not a workspace").unwrap();

    for path in [&missing, &file] {
        let output = run_with_home(&[OsStr::new("open"), path.as_os_str()], home.path());
        assert!(!output.status.success(), "path={}", path.display());
        assert!(!output.stderr.is_empty(), "path={}", path.display());
    }
}

#[test]
fn clap_errors_do_not_launch_a_tui() {
    for args in [
        &[OsStr::new("hop"), OsStr::new("extra")][..],
        &[OsStr::new("config"), OsStr::new("extra")][..],
        &[OsStr::new("open"), OsStr::new("one"), OsStr::new("two")][..],
    ] {
        let output = run(args);
        assert!(!output.status.success(), "args={args:?}");
        assert!(!stdout(&output).contains("TUI"), "args={args:?}");
        assert!(!output.stderr.is_empty(), "args={args:?}");
    }
}

#[test]
fn special_entry_argv_errors_are_rejected_before_runtime_side_effects() {
    struct Case {
        name: &'static str,
        args: &'static [&'static str],
    }

    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cases = [
        Case {
            name: "unknown daemon verb",
            args: &["daemon", "bogus"],
        },
        Case {
            name: "daemon verb with an extra argument",
            args: &["daemon", "status", "extra"],
        },
        Case {
            name: "mcp with an extra argument",
            args: &["mcp", "extra"],
        },
    ];

    // Observe every case before asserting so this regression test also cleans
    // up the daemon that the old `mcp extra` path started before reading EOF.
    let observations = cases
        .into_iter()
        .map(|case| {
            let home = short_home();
            let output = Command::new(env!("CARGO_BIN_EXE_usagi"))
                .args(case.args)
                .env("USAGI_HOME", home.path())
                .current_dir(home.path())
                .stdin(Stdio::null())
                .output()
                .expect("usagi バイナリを起動できる");
            let created_channel_data = channel_data_dir(home.path()).exists();
            if created_channel_data {
                stop_daemon(home.path());
            }
            (case, output, home, created_channel_data)
        })
        .collect::<Vec<_>>();

    for (case, output, home, created_channel_data) in observations {
        assert_eq!(output.status.code(), Some(2), "{}", case.name);
        assert!(output.stdout.is_empty(), "{}", case.name);
        assert!(stderr(&output).contains("Usage"), "{}", case.name);
        assert!(
            !created_channel_data,
            "{} created runtime data at {}",
            case.name,
            channel_data_dir(home.path()).display()
        );
    }
}

#[cfg(unix)]
#[test]
fn open_accepts_an_existing_non_utf8_workspace_path_when_supported() {
    use std::os::unix::ffi::OsStringExt;

    let home = tempfile::tempdir().unwrap();
    let roots = tempfile::tempdir().unwrap();
    let name = std::ffi::OsString::from_vec(b"usagi-\xff".to_vec());
    let path = roots.path().join(name);
    match std::fs::create_dir(&path) {
        Ok(()) => {}
        // APFS などは非 UTF-8 filename の作成・lookup 自体を拒否する。その環境では実在する
        // fixture を作れないため、この契約は非 UTF-8 filename を扱える filesystem 上で検証する。
        Err(_) if cfg!(target_os = "macos") => return,
        Err(error) => panic!("non-UTF-8 workspace fixtureを作成できない: {error}"),
    }
    let output = run_with_home(&[OsStr::new("open"), path.as_os_str()], home.path());

    assert!(output.status.success());
    assert!(stdout(&output).contains("main"));
    // JSON の path は UTF-8 string なので、非 UTF-8 path は一時 workspace として開き、
    // 壊れた registry を永続化しない。
    assert!(
        !channel_data_dir(home.path())
            .join("workspaces.json")
            .exists()
    );
}

#[cfg(unix)]
#[test]
fn open_validates_non_utf8_workspace_paths() {
    use std::os::unix::ffi::OsStringExt;

    let home = tempfile::tempdir().unwrap();
    let roots = tempfile::tempdir().unwrap();

    let missing_name = std::ffi::OsString::from_vec(b"missing-\xff".to_vec());
    let missing = roots.path().join(missing_name);
    let output = run_with_home(&[OsStr::new("open"), missing.as_os_str()], home.path());
    assert!(!output.status.success());
    assert!(!output.stderr.is_empty());

    // 相対の非 UTF-8 path も、filesystem が扱える場合は絶対 path へ解決して開ける。
    let relative = std::ffi::OsString::from_vec(b"relative-\xff".to_vec());
    let absolute_relative = roots.path().join(&relative);
    let relative_fixture_exists = std::fs::create_dir(&absolute_relative).is_ok();
    let output = Command::new(env!("CARGO_BIN_EXE_usagi"))
        .args([OsStr::new("open"), relative.as_os_str()])
        .current_dir(roots.path())
        .env("USAGI_HOME", home.path())
        .output()
        .expect("usagi バイナリを起動できる");
    if relative_fixture_exists {
        assert!(output.status.success());
        assert!(stdout(&output).contains("main"));
    } else {
        assert!(!output.status.success());
        assert!(!output.stderr.is_empty());
    }

    // 非 UTF-8 filename を扱える filesystem では、通常 file も directory と誤認しない。
    let file_name = std::ffi::OsString::from_vec(b"file-\xff".to_vec());
    let file = roots.path().join(file_name);
    match std::fs::write(&file, "not a workspace") {
        Ok(()) => {
            let output = run_with_home(&[OsStr::new("open"), file.as_os_str()], home.path());
            assert!(!output.status.success());
            assert!(!output.stderr.is_empty());
        }
        Err(_) if cfg!(target_os = "macos") => {}
        Err(error) => panic!("non-UTF-8 file fixtureを作成できない: {error}"),
    }

    // fixture を作れた環境では相対指定が実在するディレクトリへ解決された。
    if relative_fixture_exists {
        assert!(absolute_relative.is_dir());
    }
    assert!(
        !channel_data_dir(home.path())
            .join("workspaces.json")
            .exists()
    );
}

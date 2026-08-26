//! 配布バイナリの CLI 解析から TUI 起動画面までを通す結合テスト。

use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::{Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use usagi_cli::mcp::tool::{Tool, ToolError};
use usagi_cli::mcp::tools::issue::{
    IssueCreate, IssueDelete, IssueGet, IssueSearch, IssueToPrompt, IssueUpdate,
};
use usagi_core::domain::settings::{DefaultModel, LocalSettings, Settings};
use usagi_core::infrastructure::ipc::{
    BuildIdentity, DaemonGeneration, Envelope, EnvelopeKind, ErrorCode, OperationId, ProtocolError,
    ResponseOutcome, read_json_frame, write_json_frame,
};
use usagi_core::infrastructure::paths::RuntimeMode;
use usagi_core::infrastructure::store::settings::WorkspaceSettingsStore;
use usagi_core::infrastructure::store::workspace::Storage;
use usagi_daemon::infrastructure::unix_transport::{
    EndpointLocator, EndpointState, SecureUnixListener, connect_current, ensure_private_dir_all,
    read_locator,
};
use usagi_daemon::usecase::authority::registry::RegistryDocument;
use usagi_daemon::usecase::replacement::{SeamlessRefusal, seamless_refusal};

/// 起動する usagi プロセスはすべてこの fixture 経由にする。daemon の workspace root は
/// 起動時 cwd で決まるため、cwd を fixture へ固定して開発者のチェックアウトを掴ませない。
#[path = "support/daemon.rs"]
mod daemon_fixture;

use daemon_fixture::{Channel, DaemonHome};

/// Daemon lifecycle tests spawn the same test binary as a background daemon.
/// Serialize those starts so parallel integration tests cannot race its process
/// discovery and readiness publication on a loaded CI runner.
static DAEMON_LIFECYCLE_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn shipping_issue_adapters_cover_defensive_parsing_and_missing_projection() {
    for tool in [
        &IssueCreate as &dyn Tool,
        &IssueGet,
        &IssueToPrompt,
        &IssueSearch,
        &IssueUpdate,
        &IssueDelete,
    ] {
        assert!(!tool.description().is_empty());
        assert!(matches!(tool.call("{"), Err(ToolError::InvalidParams(_))));
    }
    assert_eq!(IssueGet.call(r#"{"number":4294967295}"#).unwrap(), "null");
    assert!(matches!(
        IssueToPrompt.call(r#"{"number":4294967295}"#),
        Err(ToolError::Execution(_))
    ));
}

fn short_home() -> DaemonHome {
    DaemonHome::new()
}

fn precreate_restrictive_umask_coverage_profile(command: &mut Command) {
    let Some(inherited_profile) = std::env::var_os("LLVM_PROFILE_FILE") else {
        return;
    };
    let profile_directory = Path::new(&inherited_profile)
        .parent()
        .expect("coverage profile path has a parent");
    let profile = tempfile::Builder::new()
        .prefix("usagi-restrictive-umask-")
        .suffix(".profraw")
        .tempfile_in(profile_directory)
        .expect("pre-create restrictive-umask coverage profile");
    let (file, path) = profile
        .keep()
        .expect("retain restrictive-umask coverage profile");
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .expect("make restrictive-umask coverage profile readable");
    drop(file);
    command.env("LLVM_PROFILE_FILE", path);
}

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn linked_issue_session(name: &str) -> (tempfile::TempDir, PathBuf) {
    let workspace = tempfile::tempdir().unwrap();
    git(workspace.path(), &["init", "-q"]);
    git(
        workspace.path(),
        &["config", "user.email", "cli-e2e@example.test"],
    );
    git(workspace.path(), &["config", "user.name", "CLI E2E"]);
    std::fs::write(workspace.path().join("README.md"), "fixture\n").unwrap();
    git(workspace.path(), &["add", "README.md"]);
    git(workspace.path(), &["commit", "-qm", "fixture"]);
    let sessions = workspace.path().join(".usagi/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let session = sessions.join(name);
    git(
        workspace.path(),
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            &format!("test/{name}"),
            session.to_str().unwrap(),
        ],
    );
    (workspace, session)
}

fn channel_data_dir(home: &Path) -> PathBuf {
    Channel::Local.data_dir(home)
}

fn shipping_build_identity() -> BuildIdentity {
    usagi_core::infrastructure::ipc::build_identity(
        env!("CARGO_PKG_VERSION"),
        env!("USAGI_BUILD_COMMIT"),
        env!("USAGI_BUILD_TARGET"),
        env!("USAGI_BUILD_PROFILE"),
        env!("USAGI_BUILD_SOURCE_ID"),
    )
}

fn run(home: &DaemonHome, args: &[&OsStr]) -> Output {
    home.run(args)
}

fn stop_daemon(home: &DaemonHome) {
    let output = home.run(&[OsStr::new("daemon"), OsStr::new("stop")]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_daemon_running(home: &DaemonHome) {
    let output = home.run(&[OsStr::new("daemon"), OsStr::new("status")]);
    assert!(output.status.success());
    assert!(stdout(&output).contains("daemon running"));
}

fn run_with_home(args: &[&OsStr], home: &DaemonHome) -> Output {
    home.run(args)
}

fn run_in_production(args: &[&OsStr], home: &DaemonHome) -> Output {
    home.run_in_production(args)
}

fn daemon_pid(home: &Path) -> Option<u32> {
    daemon_record(home).map(|record| record.pid)
}

fn daemon_record(home: &Path) -> Option<usagi_core::domain::daemon::DaemonRecord> {
    let bytes = std::fs::read(home.join("daemon/daemon.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn process_alive(pid: u32) -> bool {
    libc::pid_t::try_from(pid).is_ok_and(|pid| unsafe { libc::kill(pid, 0) } == 0)
}

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    condition()
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

/// A scripted daemon on the real Unix transport.
///
/// `workspace_root` is the workspace this fake daemon claims authority over. The
/// handshake fence compares a client's declared workspace against it, so a
/// fixture that wants its `usagi` client admitted passes the same workspace the
/// client runs in (#548).
fn spawn_fake_daemon(
    home: &Path,
    workspace_root: &Path,
    reply: FakeDaemonReply,
) -> thread::JoinHandle<()> {
    let data_dir = channel_data_dir(home);
    let workspace_root = usagi_core::infrastructure::paths::wire_workspace_root(
        usagi_core::infrastructure::paths::canonical_workspace_root(workspace_root).unwrap(),
    );
    ensure_private_dir_all(&data_dir).unwrap();
    let generation = DaemonGeneration(format!("fake-{}", std::process::id()));
    let listener = SecureUnixListener::bind(&data_dir, generation.clone()).unwrap();
    let record = usagi_core::domain::daemon::DaemonRecord::identified(
        std::process::id(),
        daemon_fixture::process_start_identity(std::process::id()),
    );
    std::fs::write(
        data_dir.join("daemon/daemon.json"),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
    let server_build = shipping_build_identity();
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
            server_build,
            record,
            workspace_root,
        );
        // A refused handshake (e.g. the workspace fence) writes its typed error
        // frame and ends the connection; there is no request to script after it.
        let Some(hello) =
            usagi_daemon::presentation::ipc::handshake(&mut stream, &mut writer, &server).unwrap()
        else {
            return;
        };
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
    ensure_private_dir_all(&generation_dir).unwrap();
    for directory in [&daemon, &daemon.join("generations"), &generation_dir] {
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let socket = generation_dir.join("sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    drop(listener);
    std::fs::remove_file(&socket).unwrap();
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

fn run_mcp(home: &DaemonHome, cwd: &Path, requests: &str) -> Output {
    let mut child = home
        .command_at(Channel::Local, cwd, &[OsStr::new("mcp")])
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
        let output = run_with_home(args, &home);
        assert!(output.status.success(), "args={args:?}");
        let out = stdout(&output);
        assert!(out.contains("USAGI"), "args={args:?}");
        assert!(out.contains("Menu"), "args={args:?}");
        assert!(out.contains("q: quit"), "args={args:?}");
        assert!(output.stderr.is_empty(), "args={args:?}");
    }
    stop_daemon(&home);
}

#[test]
fn daemon_status_reports_not_running_with_a_fresh_data_dir() {
    // `usagi daemon status` を実バイナリで走らせ、合成ルートが束ねる実ストア
    // （`FsRecordFile` を backing にした `DaemonRecordStore`）を通す。データディレクトリを
    // 空の一時パスへ向けるので、レコードは無く「daemon not running」を報告する。
    let home = short_home();
    let output = home.run(&[OsStr::new("daemon"), OsStr::new("status")]);
    assert!(output.status.success());
    assert!(stdout(&output).contains("daemon not running"));
}

#[test]
fn daemon_stop_clears_a_stale_production_record() {
    let home = short_home();
    let daemon_dir = home.path().join("daemon");
    std::fs::create_dir(&daemon_dir).unwrap();
    std::fs::set_permissions(&daemon_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let record_path = daemon_dir.join("daemon.json");
    let record = usagi_core::domain::daemon::DaemonRecord::identified(2_000_000_000, "test:absent");
    std::fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();
    std::fs::set_permissions(&record_path, std::fs::Permissions::from_mode(0o600)).unwrap();

    let output = run_in_production(&[OsStr::new("daemon"), OsStr::new("stop")], &home);

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "usagi v{}: cleared stale daemon record\n",
            env!("CARGO_PKG_VERSION")
        )
    );
    assert!(!record_path.exists());
    assert_eq!(
        std::fs::metadata(daemon_dir.join("record.lock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

/// A crashed daemon leaves its record, locator, and socket behind. Once the OS
/// hands its pid to an unrelated process, the recorded identity stops matching —
/// which is positive proof the owner is gone, not an unknown owner. The explicit
/// lifecycle commands must reclaim that record themselves instead of wedging
/// until some unrelated daemon-backed request happens to run.
#[test]
fn daemon_lifecycle_recovers_a_crash_record_whose_pid_was_reused() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let daemon_dir = home.production_data_dir().join("daemon");
    let mut crashed = home.spawn_serve();
    assert!(
        wait_until(Duration::from_secs(15), || {
            daemon_dir.join("daemon.json").is_file() && daemon_dir.join("current.json").is_file()
        }),
        "daemon did not publish its production endpoint"
    );
    let socket = daemon_dir.join(
        &read_locator(&daemon_dir)
            .expect("a started daemon publishes a locator")
            .endpoint,
    );
    let crashed_record = daemon_record(home.path()).expect("the daemon registered a record");
    crashed.kill_and_reap();
    assert!(socket.exists(), "SIGKILL leaves the endpoint behind");

    // Model the PID reuse: an unrelated live process now occupies the recorded
    // pid, so only the recorded identity distinguishes it from the dead owner.
    let mut occupant = Command::new("sleep").arg("30").spawn().unwrap();
    let reused = usagi_core::domain::daemon::DaemonRecord {
        pid: occupant.id(),
        process_start_identity: crashed_record.process_start_identity.clone(),
        started_at: crashed_record.started_at,
    };
    std::fs::write(
        daemon_dir.join("daemon.json"),
        serde_json::to_vec(&reused).unwrap(),
    )
    .unwrap();

    // `status` names the reuse rather than only calling the record stale.
    let status = run_in_production(&[OsStr::new("daemon"), OsStr::new("status")], &home);
    assert!(status.status.success(), "{}", stderr(&status));
    assert!(
        stdout(&status).contains(&format!(
            "daemon not running (stale record, pid {} was reused by another process; reclaimable)",
            reused.pid
        )),
        "status: {}",
        stdout(&status)
    );

    // `stop` reclaims the record and the crashed endpoint, and sends no signal to
    // the process that now holds the pid.
    let stop = run_in_production(&[OsStr::new("daemon"), OsStr::new("stop")], &home);
    assert!(stop.status.success(), "{}", stderr(&stop));
    assert!(stdout(&stop).contains("cleared stale daemon record"));
    assert!(!daemon_dir.join("daemon.json").exists());
    assert!(!daemon_dir.join("current.json").exists());
    assert!(!socket.exists());
    assert!(
        occupant.try_wait().unwrap().is_none(),
        "the reclaim signalled the unrelated process holding the reused pid"
    );

    // `start` then launches one replacement, which registers its own identity.
    let start = run_in_production(&[OsStr::new("daemon"), OsStr::new("start")], &home);
    assert!(start.status.success(), "{}", stderr(&start));
    let started = daemon_record(home.path()).expect("start registers a record");
    assert_ne!(started.pid, reused.pid);
    assert_eq!(
        started.process_start_identity.as_deref(),
        Some(daemon_fixture::process_start_identity(started.pid).as_str())
    );
    assert!(occupant.try_wait().unwrap().is_none());

    // The recovered daemon answers ordinary bootstrap, so no client adds a second
    // one on top of it.
    let request = run_in_production(
        &[
            OsStr::new("session"),
            OsStr::new("remove"),
            OsStr::new("missing"),
        ],
        &home,
    );
    assert_eq!(request.status.code(), Some(1));
    assert_eq!(daemon_record(home.path()), Some(started));

    occupant.kill().unwrap();
    occupant.wait().unwrap();
    let stop = run_in_production(&[OsStr::new("daemon"), OsStr::new("stop")], &home);
    assert!(stop.status.success(), "{}", stderr(&stop));
}

#[test]
fn daemon_restart_initializes_a_private_endpoint_from_an_empty_data_dir() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let output = home.run(&[OsStr::new("daemon"), OsStr::new("restart")]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout(&output).contains("daemon restarted"));
    assert_daemon_running(&home);
    stop_daemon(&home);
}

/// The shipping daemon registers itself in the durable generation registry, and
/// the registry is what makes the published locator meaningful: `current.json`
/// names an endpoint, `generations.json` says which generation owns it and that
/// it holds authority. A rollover has nothing to hand authority *from* until both
/// exist, which is why this is asserted on the real binary rather than a fixture.
#[test]
fn a_started_daemon_registers_its_generation_and_retires_it_on_stop() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let daemon_dir = home.data_dir().join("daemon");
    let registry = daemon_dir.join("generations.json");

    let start = home.run(&[OsStr::new("daemon"), OsStr::new("start")]);
    assert!(start.status.success(), "{}", stderr(&start));
    assert!(
        wait_until(Duration::from_secs(15), || registry.is_file()
            && daemon_dir.join("current.json").is_file()),
        "daemon did not register a generation next to its locator"
    );

    let locator = read_locator(&daemon_dir).expect("a started daemon publishes a locator");
    let document = registry_document(&registry);
    let generations = document["generations"]
        .as_array()
        .expect("the registry lists generations");
    assert_eq!(generations.len(), 1, "{document}");
    let entry = &generations[0];
    assert_eq!(document["current"], entry["generation"], "{document}");
    assert_eq!(entry["role"], "active", "{document}");
    // One spelling of the endpoint: the registry entry and the locator must name
    // the same socket, or a client that resolved either would reach a different
    // daemon than the other names.
    assert_eq!(entry["endpoint"], locator.endpoint.as_str(), "{document}");
    assert_eq!(
        entry["generation"],
        locator.generation.0.as_str(),
        "{document}"
    );
    // The recorded process identity is the same token `daemon.json` carries, so a
    // later start can prove whether this authority is still alive.
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(daemon_dir.join("daemon.json")).unwrap()).unwrap();
    assert_eq!(
        entry["process"]["start_identity"], record["process_start_identity"],
        "{document}"
    );

    stop_daemon(&home);
    // A clean stop gives the authority up rather than leaving a generation the
    // next start would have to fail closed. The daemon releases it on its own way
    // out, so the observable state is reached asynchronously.
    assert!(
        wait_until(Duration::from_secs(10), || {
            let document = registry_document(&registry);
            document["current"].is_null() && document["generations"][0]["role"] == "retired"
        }),
        "a stopped daemon kept its registry authority: {}",
        registry_document(&registry)
    );
}

/// A daemon that is restarted twice keeps the registry bounded: a retired
/// generation is already unaddressable, so retaining its record forever would
/// only grow the document one entry per restart.
#[test]
fn repeated_restarts_leave_exactly_one_registered_generation() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let registry = home.data_dir().join("daemon/generations.json");
    let mut generations = Vec::new();

    for _ in 0..3 {
        let restart = home.run(&[OsStr::new("daemon"), OsStr::new("restart")]);
        assert!(restart.status.success(), "{}", stderr(&restart));
        let previous = generations.last().cloned();
        assert!(
            wait_until(Duration::from_secs(15), || {
                registry.is_file() && {
                    let current = registry_document(&registry)["current"].clone();
                    !current.is_null() && Some(&current) != previous.as_ref()
                }
            }),
            "restart did not register a new active generation"
        );
        let document = registry_document(&registry);
        // Exactly one entry, restart after restart: a retired generation is
        // already unaddressable, so keeping its record would only grow the
        // document once per restart forever.
        assert_eq!(
            document["generations"].as_array().map(Vec::len),
            Some(1),
            "{document}"
        );
        generations.push(document["current"].clone());
    }

    generations.dedup();
    assert_eq!(generations.len(), 3, "each restart is a new generation");
    stop_daemon(&home);
}

/// A standby process is the second daemon in one data directory. Everything this
/// asserts is about what it does *not* do: it takes neither guard, writes no
/// lifecycle record, and above all leaves `current.json` naming the active
/// generation, so no client can be routed to it. What it does do is become a
/// registered standby whose artifact was verified after readiness — which is
/// exactly what a rollover needs to be able to name a successor.
#[test]
fn a_standby_registers_beside_the_active_generation_without_publishing_a_locator() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let daemon_dir = home.production_data_dir().join("daemon");
    let registry = daemon_dir.join("generations.json");

    let mut active = home.spawn_serve();
    assert!(
        wait_until(Duration::from_secs(15), || registry.is_file()
            && daemon_dir.join("current.json").is_file()),
        "the active daemon did not register its generation"
    );
    let published = std::fs::read(daemon_dir.join("current.json")).unwrap();
    let record = std::fs::read(daemon_dir.join("daemon.json")).unwrap();

    let mut standby = home.spawn_standby();
    assert!(
        wait_until(Duration::from_secs(20), || {
            standby_entry(&registry).is_some_and(|entry| entry["verified_build"].is_object())
        }),
        "the standby never reached verified readiness: {}",
        registry_document(&registry)
    );

    let document = registry_document(&registry);
    let entry = standby_entry(&registry).expect("the standby is registered");
    // Two retained generations, one active and one standby, and `current` still
    // names the active one. A standby that moved `current` would have handed
    // itself authority nobody granted.
    assert_eq!(document["generations"].as_array().map(Vec::len), Some(2));
    assert_ne!(document["current"], entry["generation"], "{document}");
    assert_eq!(
        std::fs::read(daemon_dir.join("current.json")).unwrap(),
        published,
        "the standby republished the current locator"
    );
    // The owner record is the active daemon's alone: a standby owns nothing, so
    // it registers nothing about who owns the data directory.
    assert_eq!(
        std::fs::read(daemon_dir.join("daemon.json")).unwrap(),
        record
    );
    assert_eq!(
        entry["verified_build"], entry["expected_build"],
        "{document}"
    );
    // The registry entry names a socket that is actually accepting: readiness
    // completed a handshake against this exact path.
    let socket = daemon_dir.join(entry["endpoint"].as_str().unwrap());
    assert!(socket.exists(), "the standby endpoint is not bound");
    // The refusal a rollover reports is now an *observation* of this document
    // rather than the constant it used to be. Naming a verified standby is the
    // whole point of registering one; enabling the rollover itself is #559.
    assert_eq!(
        seamless_refusal(
            Some(
                &serde_json::from_slice::<RegistryDocument>(&std::fs::read(&registry).unwrap())
                    .expect("the shipping daemon writes a registry this build understands")
            ),
            true,
            2
        ),
        Some(SeamlessRefusal::GenerationLimit)
    );

    // Standing the standby down gives up the entry and the socket, and leaves the
    // active generation exactly as it was.
    assert!(standby.terminate_and_wait(), "the standby ignored SIGTERM");
    assert!(
        wait_until(Duration::from_secs(10), || {
            standby_entry(&registry).is_none_or(|entry| entry["role"] == "retired")
                && !socket.exists()
        }),
        "a stopped standby kept its registry entry or its socket: {}",
        registry_document(&registry)
    );
    let document = registry_document(&registry);
    assert_eq!(
        Some(&document["current"]),
        published_generation(&daemon_dir).as_ref()
    );
    // The active daemon was never disturbed by any of it.
    assert!(!active.wait_for_exit(Duration::from_millis(1)));
    stop_daemon_in_production(&home);
}

/// A standby is a *retained* generation, and activation refuses a registry that
/// retains one. So a standby that outlives its incumbent does not merely idle —
/// it refuses every future `daemon start` in this data directory with
/// `authority_retained`, forever, until someone kills it by hand.
///
/// The crash path happens to be safe on its own (recovery fails the abandoned
/// authority closed and retires *every* generation, which the standby notices),
/// so this pins the path that is not: an ordinary, clean `daemon stop`, which
/// retires only the active's own entry.
#[test]
fn a_standby_stands_down_with_its_incumbent_so_the_next_start_succeeds() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let daemon_dir = home.production_data_dir().join("daemon");
    let registry = daemon_dir.join("generations.json");

    let mut active = home.spawn_serve();
    assert!(
        wait_until(Duration::from_secs(15), || registry.is_file()
            && daemon_dir.join("current.json").is_file()),
        "the active daemon did not register its generation"
    );
    let mut standby = home.spawn_standby();
    assert!(
        wait_until(Duration::from_secs(20), || {
            standby_entry(&registry).is_some_and(|entry| entry["verified_build"].is_object())
        }),
        "the standby never reached verified readiness: {}",
        registry_document(&registry)
    );

    // A clean stop, not a kill: the active gives up its own entry and nothing
    // else in the registry changes.
    stop_daemon_in_production(&home);
    assert!(
        active.wait_for_exit(Duration::from_secs(10)),
        "the active daemon did not exit"
    );

    assert!(
        standby.wait_for_exit(Duration::from_secs(20)),
        "the standby outlived the authority it was admitted to succeed: {}",
        registry_document(&registry)
    );
    assert!(
        wait_until(Duration::from_secs(10), || {
            registry_document(&registry)["generations"]
                .as_array()
                .is_some_and(|entries| entries.iter().all(|entry| entry["role"] == "retired"))
        }),
        "a retained generation survived both daemons: {}",
        registry_document(&registry)
    );

    // The whole point: activation is possible again without manual cleanup.
    let restarted = run_in_production(&[OsStr::new("daemon"), OsStr::new("start")], &home);
    assert!(restarted.status.success(), "{}", stderr(&restarted));
    assert!(
        wait_until(Duration::from_secs(15), || {
            !registry_document(&registry)["current"].is_null()
        }),
        "the next start could not take authority: {}",
        registry_document(&registry)
    );
    stop_daemon_in_production(&home);
}

/// The standby's own custody supervisor is what stands it down when its
/// incumbent goes away — and a killed standby has no supervisor left to run it.
/// Its `standby` entry therefore survives with nobody to revisit it: recovery
/// reconciles the *active* against the locator and never looks at the rest, so
/// once the active has cleanly retired its own entry the leftover is the only
/// retained generation — and `activate_first` refuses while any generation is
/// retained. Every subsequent `daemon start` would fail `authority_retained`
/// forever, until someone deleted `generations.json` by hand.
#[test]
fn a_killed_standby_does_not_wedge_every_later_start() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let daemon_dir = home.production_data_dir().join("daemon");
    let registry = daemon_dir.join("generations.json");

    let mut active = home.spawn_serve();
    assert!(
        wait_until(Duration::from_secs(15), || registry.is_file()
            && daemon_dir.join("current.json").is_file()),
        "the active daemon did not register its generation"
    );
    let mut standby = home.spawn_standby();
    assert!(
        wait_until(Duration::from_secs(20), || {
            standby_entry(&registry).is_some_and(|entry| entry["verified_build"].is_object())
        }),
        "the standby never reached verified readiness: {}",
        registry_document(&registry)
    );

    // SIGKILL, so nothing on the standby's side runs: no stand-down, no custody
    // tick, no entry retirement.
    standby.kill_and_reap();
    assert!(
        standby_entry(&registry).is_some_and(|entry| entry["role"] == "standby"),
        "the killed standby was expected to leave its entry behind: {}",
        registry_document(&registry)
    );

    // A clean stop retires only the active's own entry, which leaves the dead
    // standby as the single retained generation.
    stop_daemon_in_production(&home);
    assert!(
        active.wait_for_exit(Duration::from_secs(10)),
        "the active daemon did not exit"
    );
    assert_eq!(
        registry_document(&registry)["generations"]
            .as_array()
            .map(|entries| entries
                .iter()
                .filter(|entry| entry["role"] != "retired")
                .count()),
        Some(1),
        "{}",
        registry_document(&registry)
    );

    // The next start proves the leftover's process is gone, reclaims it, and
    // takes the authority — no manual cleanup anywhere.
    let restarted = run_in_production(&[OsStr::new("daemon"), OsStr::new("start")], &home);
    assert!(restarted.status.success(), "{}", stderr(&restarted));
    assert!(
        wait_until(Duration::from_secs(15), || {
            !registry_document(&registry)["current"].is_null()
        }),
        "the next start could not take authority: {}",
        registry_document(&registry)
    );
    let document = registry_document(&registry);
    assert_eq!(document["generations"].as_array().map(Vec::len), Some(1));
    assert_eq!(document["generations"][0]["role"], "active", "{document}");
    stop_daemon_in_production(&home);
}

/// A standby is not a way to start serving. Without a live daemon that the
/// registry itself names as active there is nothing to stand by for, and the
/// refusal has to land before anything is created inside a data directory this
/// process does not own.
#[test]
fn a_standby_is_refused_when_no_registered_active_owns_the_data_directory() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let daemon_dir = home.production_data_dir().join("daemon");
    let registry = daemon_dir.join("generations.json");

    // A data directory no daemon has ever taken authority over.
    let fresh = run_in_production(
        &[
            OsStr::new("daemon"),
            OsStr::new("serve"),
            OsStr::new("--standby"),
        ],
        &home,
    );
    assert_eq!(fresh.status.code(), Some(1), "{}", stderr(&fresh));
    assert!(
        stderr(&fresh).contains("no generation registry exists"),
        "{}",
        stderr(&fresh)
    );
    assert!(!registry.exists(), "a refused standby wrote the registry");
    assert!(!daemon_dir.join("generations").exists());

    // A data directory whose daemon has stopped: the registry exists and names a
    // retired generation, and there is no owner record at all.
    let start = run_in_production(&[OsStr::new("daemon"), OsStr::new("start")], &home);
    assert!(start.status.success(), "{}", stderr(&start));
    assert!(wait_until(Duration::from_secs(15), || registry.is_file()));
    stop_daemon_in_production(&home);
    assert!(wait_until(Duration::from_secs(10), || {
        registry_document(&registry)["current"].is_null()
    }));
    let before = std::fs::read(&registry).unwrap();

    let stopped = run_in_production(
        &[
            OsStr::new("daemon"),
            OsStr::new("serve"),
            OsStr::new("--standby"),
        ],
        &home,
    );
    assert_eq!(stopped.status.code(), Some(1), "{}", stderr(&stopped));
    assert!(
        stderr(&stopped).contains("no live daemon owns this data directory"),
        "{}",
        stderr(&stopped)
    );
    // Effect zero: not one byte of the registry moved, and no endpoint was bound.
    assert_eq!(std::fs::read(&registry).unwrap(), before);
}

/// One data directory holds one active generation. The workspace fence refuses
/// the second `serve` before it touches anything, and the registry it did not
/// reach is byte-identical afterwards — the two guards agree, and neither one
/// alone is what the refusal rests on.
#[test]
fn a_second_active_daemon_is_refused_without_disturbing_the_registry() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let daemon_dir = home.production_data_dir().join("daemon");
    let registry = daemon_dir.join("generations.json");

    let mut owner = home.spawn_serve();
    assert!(
        wait_until(Duration::from_secs(15), || registry.is_file()
            && daemon_dir.join("current.json").is_file()),
        "the first daemon did not take authority"
    );
    let before = std::fs::read(&registry).unwrap();
    let published = std::fs::read(daemon_dir.join("current.json")).unwrap();

    let second = run_in_production(&[OsStr::new("daemon"), OsStr::new("serve")], &home);
    assert!(
        stdout(&second).contains("another daemon already owns this workspace"),
        "{}",
        stdout(&second)
    );
    assert_eq!(std::fs::read(&registry).unwrap(), before);
    assert_eq!(
        std::fs::read(daemon_dir.join("current.json")).unwrap(),
        published
    );
    assert!(!owner.wait_for_exit(Duration::from_millis(1)));
    stop_daemon_in_production(&home);
}

/// A `SIGKILL`ed active daemon leaves a registry entry naming a process that no
/// longer exists. Nothing may adopt that entry as an authority, and nothing may
/// be blocked by it either: the next start proves the process gone, retires the
/// entry, and activates in its place.
#[test]
fn a_killed_active_leaves_a_stale_entry_the_next_start_reclaims() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let daemon_dir = home.production_data_dir().join("daemon");
    let registry = daemon_dir.join("generations.json");

    let mut killed = home.spawn_serve();
    assert!(
        wait_until(Duration::from_secs(15), || registry.is_file()
            && daemon_dir.join("current.json").is_file()),
        "the daemon did not take authority before being killed"
    );
    let stale = registry_document(&registry)["current"].clone();
    assert!(!stale.is_null());
    killed.kill_and_reap();

    // The durable state is exactly the crash matrix's "after W2" row: an active
    // entry naming a dead process, and a locator naming it.
    assert_eq!(registry_document(&registry)["current"], stale);
    assert!(daemon_dir.join("current.json").exists());

    let restart = run_in_production(&[OsStr::new("daemon"), OsStr::new("start")], &home);
    assert!(restart.status.success(), "{}", stderr(&restart));
    assert!(
        wait_until(Duration::from_secs(15), || {
            let document = registry_document(&registry);
            !document["current"].is_null()
                && document["current"] != stale
                // The locator is written after the registry commit, so both have
                // to agree before the reclamation is complete.
                && published_generation(&daemon_dir).as_ref() == Some(&document["current"])
        }),
        "the stale entry was never reclaimed: {}",
        registry_document(&registry)
    );
    let document = registry_document(&registry);
    // The dead generation is not merely retired but gone: a retired record says
    // nothing a client can act on, so keeping it would only grow the document.
    assert_eq!(document["generations"].as_array().map(Vec::len), Some(1));
    assert_eq!(document["generations"][0]["role"], "active");
    assert_eq!(
        Some(&document["current"]),
        published_generation(&daemon_dir).as_ref()
    );
    stop_daemon_in_production(&home);
}

/// The retained entry `current` does not name: with one active generation and one
/// standby, that is the standby — before and after it is retired.
fn standby_entry(registry: &Path) -> Option<serde_json::Value> {
    let document = registry_document(registry);
    let current = document["current"].clone();
    document["generations"]
        .as_array()?
        .iter()
        .find(|entry| entry["generation"] != current)
        .cloned()
}

/// The generation the published locator names, when a locator is published.
///
/// Publication follows the registry commit, so a reader that has just seen a new
/// active generation can legitimately find no locator yet.
fn published_generation(daemon_dir: &Path) -> Option<serde_json::Value> {
    let bytes = std::fs::read(daemon_dir.join("current.json")).ok()?;
    let locator: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    Some(locator["generation"].clone())
}

fn stop_daemon_in_production(home: &DaemonHome) {
    let output = run_in_production(&[OsStr::new("daemon"), OsStr::new("stop")], home);
    assert!(output.status.success(), "{}", stderr(&output));
}

/// The durable registry document, read as the daemon wrote it.
fn registry_document(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).expect("the registry document exists"))
        .expect("the registry document is JSON")
}

/// A daemon is started detached, so nothing reaps it when its launcher dies
/// abnormally. Removing its single-instance lock takes away its custody of the
/// data directory, and it must then exit on its own through the ordinary
/// graceful path — retiring its endpoint and clearing its record.
#[test]
fn a_daemon_that_loses_its_instance_lock_shuts_itself_down_gracefully() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let mut abandoned = home.spawn_serve();
    let daemon_dir = home.path().join("daemon");
    assert!(
        wait_until(Duration::from_secs(15), || {
            daemon_dir.join("daemon.json").is_file() && daemon_dir.join("current.json").is_file()
        }),
        "daemon did not publish its production endpoint"
    );
    let locator = read_locator(&daemon_dir).expect("started daemon publishes a locator");
    let socket = daemon_dir.join(&locator.endpoint);
    assert!(socket.exists());

    std::fs::remove_file(daemon_dir.join("daemon.lock")).unwrap();

    assert!(
        abandoned.wait_for_exit(Duration::from_secs(10)),
        "a daemon that lost custody of its data directory kept running"
    );
    assert!(
        wait_until(Duration::from_secs(5), || {
            !daemon_dir.join("daemon.json").exists()
                && !daemon_dir.join("current.json").exists()
                && !socket.exists()
        }),
        "self-shutdown left its record, locator, or generation socket behind"
    );
    // Self-shutdown never re-creates the fence it lost.
    assert!(!daemon_dir.join("daemon.lock").exists());
}

/// The same custody loss, but with the whole data directory deleted underneath
/// the daemon (a `$USAGI_HOME` temporary directory removed by a dead test
/// harness). Cleanup must be a silent no-op instead of resurrecting the tree.
#[test]
fn a_daemon_whose_data_directory_is_deleted_exits_without_re_creating_it() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let mut abandoned = home.spawn_serve_in(Channel::Local);
    let data_dir = home.data_dir();
    assert!(
        wait_until(Duration::from_secs(15), || {
            data_dir.join("daemon/daemon.json").is_file()
                && data_dir.join("daemon/current.json").is_file()
        }),
        "daemon did not publish its local endpoint"
    );

    // The daemon is still running while the tree is removed, so a worker can
    // legitimately re-create an entry mid-walk. Retry until the tree is gone.
    assert!(
        wait_until(Duration::from_secs(5), || {
            std::fs::remove_dir_all(&data_dir).is_ok() && !data_dir.exists()
        }),
        "could not delete the daemon's data directory"
    );

    assert!(
        abandoned.wait_for_exit(Duration::from_secs(10)),
        "a daemon whose data directory disappeared kept running"
    );
    // Endpoint retirement and record clearing are no-ops on a released tree: the
    // exiting daemon must not re-create the lifecycle artifacts it lost.
    assert!(
        !data_dir.join("daemon/daemon.json").exists()
            && !data_dir.join("daemon/current.json").exists(),
        "self-shutdown re-created lifecycle artifacts under the released data directory"
    );
}

/// A daemon's authority is a workspace: its git worktrees, `usagi/<name>`
/// branches, and session names. The instance lock only fences a *data directory*,
/// which the environment selects — so re-spelling `USAGI_RUNTIME_MODE` or
/// `$USAGI_HOME` reaches a free instance lock every time. The workspace fence is
/// what makes the second start refuse instead of becoming a rival authority over
/// the same worktrees.
#[test]
fn a_second_daemon_for_the_same_workspace_is_refused_across_modes_and_data_homes() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let owner_home = short_home();
    let owner = owner_home.spawn_serve();
    let daemon_dir = owner_home.path().join("daemon");
    assert!(
        wait_until(Duration::from_secs(15), || {
            daemon_dir.join("daemon.json").is_file() && daemon_dir.join("current.json").is_file()
        }),
        "daemon did not publish its production endpoint"
    );
    let workspace =
        usagi_core::infrastructure::paths::canonical_workspace_root(owner_home.workspace())
            .unwrap();
    let expected = format!(
        "another daemon already owns this workspace ({}, pid {})",
        workspace.display(),
        owner.pid()
    );

    // Same workspace, different runtime mode: `<home>/local` has its own free
    // instance lock, and the fence is the only thing that can refuse this.
    let refused = owner_home.run(&[OsStr::new("daemon"), OsStr::new("serve")]);
    assert!(refused.status.success(), "{}", stderr(&refused));
    assert!(stdout(&refused).contains(&expected), "{}", stdout(&refused));

    // Same workspace, different `$USAGI_HOME`: likewise a free instance lock in a
    // wholly separate data directory, and likewise refused.
    let other_home = short_home();
    let refused = other_home
        .command_at(
            Channel::Local,
            owner_home.workspace(),
            &[OsStr::new("daemon"), OsStr::new("serve")],
        )
        .output()
        .expect("usagi バイナリを起動できる");
    assert!(refused.status.success(), "{}", stderr(&refused));
    assert!(stdout(&refused).contains(&expected), "{}", stdout(&refused));

    // Fencing is per workspace, not global: a daemon for a different workspace
    // still starts, so parallel workspaces (and parallel tests) keep working.
    let mut elsewhere = other_home.spawn_serve_in(Channel::Local);
    let elsewhere_dir = other_home.data_dir().join("daemon");
    assert!(
        wait_until(Duration::from_secs(15), || {
            elsewhere_dir.join("daemon.json").is_file()
        }),
        "a daemon for a different workspace was refused"
    );
    elsewhere.kill_and_reap();

    // Every refusal left the owner and its endpoint untouched.
    assert!(daemon_dir.join("daemon.json").is_file());
    assert!(daemon_dir.join("current.json").is_file());
}

/// The daemon's workspace root is its startup directory, so a test that starts
/// one without a fixture cwd binds it to the developer's checkout and blocks
/// that worktree's removal. Every start goes through the fixture, and the root
/// it recorded proves the binding.
#[test]
fn a_client_started_daemon_binds_the_fixture_workspace_root() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    // An ordinary daemon-backed request; the daemon is autostarted by bootstrap
    // rather than by an explicit lifecycle command.
    let output = run_with_home(
        &[
            OsStr::new("session"),
            OsStr::new("remove"),
            OsStr::new("missing"),
        ],
        &home,
    );
    assert_eq!(output.status.code(), Some(1));
    assert_daemon_running(&home);

    let state: serde_json::Value = serde_json::from_slice(
        &std::fs::read(daemon_fixture::lifecycle_state_path(&home.data_dir())).unwrap(),
    )
    .unwrap();
    assert_eq!(
        std::fs::canonicalize(state["repository_root"].as_str().unwrap()).unwrap(),
        std::fs::canonicalize(home.workspace()).unwrap()
    );
    stop_daemon(&home);
}

/// `daemon replace` performs the replacement its trigger keys, on exactly the
/// path `daemon restart` takes — there is no second, unguarded route to
/// `stop` → fresh `start` (#507).
#[test]
fn explicit_artifact_replacement_runs_under_one_coalesced_operation() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let mut cleanup = home.spawn_serve();
    let daemon_dir = home.path().join("daemon");
    assert!(
        wait_until(Duration::from_secs(5), || {
            daemon_dir.join("daemon.json").is_file() && daemon_dir.join("current.json").is_file()
        }),
        "daemon did not publish its production endpoint"
    );
    let old_pid = cleanup.pid();
    let old_locator = read_locator(&daemon_dir).unwrap();

    let first = run_in_production(&[OsStr::new("daemon"), OsStr::new("replace")], &home);
    assert!(first.status.success(), "{}", stderr(&first));
    // The daemon owns no runtime, so the replacement is a cold transition: the
    // old owner exits and a fresh one publishes its own endpoint.
    assert!(
        cleanup.wait_for_exit(Duration::from_secs(5)),
        "the replacement did not exit the old daemon process"
    );
    assert!(wait_until(Duration::from_secs(5), || {
        daemon_pid(home.path()).is_some_and(|pid| pid != old_pid)
    }));
    let replaced_pid = daemon_pid(home.path()).unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || {
            read_locator(&daemon_dir).is_ok_and(|locator| locator != old_locator)
        }),
        "the fresh daemon did not publish its own endpoint"
    );

    // Two invocations against the same artifact pair and channel derive the
    // same durable operation, so the transition is always attributable to one
    // key rather than to a fresh identity each time.
    let second = run_in_production(&[OsStr::new("daemon"), OsStr::new("replace")], &home);
    assert!(second.status.success(), "{}", stderr(&second));
    let operation = |output: &std::process::Output| {
        stdout(output)
            .split("(operation ")
            .nth(1)
            .and_then(|tail| tail.split(')').next())
            .map(str::to_owned)
            .expect("the replacement reports its operation")
    };
    assert_eq!(operation(&first), operation(&second));
    assert!(operation(&first).starts_with("build-rollover-v1-"));
    assert!(wait_until(Duration::from_secs(5), || {
        daemon_pid(home.path()).is_some_and(|pid| pid != replaced_pid)
    }));
}

#[test]
fn planned_stop_retires_generation_endpoint_and_allows_safe_autostart() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let mut cleanup = home.spawn_serve();
    let daemon_dir = home.path().join("daemon");

    assert!(
        wait_until(Duration::from_secs(5), || {
            daemon_dir.join("daemon.json").is_file() && daemon_dir.join("current.json").is_file()
        }),
        "daemon did not publish its production endpoint"
    );
    let old_record = daemon_record(home.path()).expect("started daemon records its identity");
    let old_pid = old_record.pid;
    assert_eq!(old_pid, cleanup.pid());
    let old_locator = read_locator(&daemon_dir).expect("started daemon publishes a locator");
    let old_socket = daemon_dir.join(&old_locator.endpoint);
    assert!(old_socket.exists());
    for private_file in [
        "daemon.json",
        "daemon.lock",
        "record.lock",
        "current.json",
        "current.lock",
    ] {
        assert_eq!(
            std::fs::metadata(daemon_dir.join(private_file))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "{private_file} is not private"
        );
    }

    let stop = run_in_production(&[OsStr::new("daemon"), OsStr::new("stop")], &home);
    assert!(stop.status.success(), "{}", stderr(&stop));
    assert!(
        cleanup.wait_for_exit(Duration::from_secs(5)),
        "planned stop did not exit its owned daemon process"
    );
    assert!(
        wait_until(Duration::from_secs(5), || {
            !daemon_dir.join("daemon.json").exists()
                && !daemon_dir.join("current.json").exists()
                && !old_socket.exists()
        }),
        "planned stop left its process, record, locator, or generation socket behind"
    );
    assert_eq!(
        connect_current(home.path()).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );

    let client = run_in_production(
        &[
            OsStr::new("session"),
            OsStr::new("remove"),
            OsStr::new("missing"),
        ],
        &home,
    );
    assert_eq!(client.status.code(), Some(1));
    assert!(
        stderr(&client).contains("session was not found"),
        "{}",
        stderr(&client)
    );
    assert!(!stderr(&client).contains("daemon endpoint is unavailable"));
    assert!(
        wait_until(Duration::from_secs(5), || {
            daemon_pid(home.path()).is_some() && daemon_dir.join("current.json").is_file()
        }),
        "NotFound bootstrap did not start a replacement daemon"
    );
    let replacement = read_locator(&daemon_dir).expect("replacement publishes a locator");
    let replacement_pid = daemon_pid(home.path()).expect("replacement records its pid");
    assert_ne!(replacement.generation, old_locator.generation);
    assert!(!old_socket.exists());

    let stop = run_in_production(&[OsStr::new("daemon"), OsStr::new("stop")], &home);
    assert!(stop.status.success(), "{}", stderr(&stop));
    assert!(
        wait_until(Duration::from_secs(5), || {
            !process_alive(replacement_pid)
                && !daemon_dir.join("daemon.json").exists()
                && !daemon_dir.join("current.json").exists()
        }),
        "replacement daemon did not stop cleanly"
    );
}

#[test]
fn ordinary_client_recovers_a_sigkilled_daemon_without_manual_lifecycle() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let mut killed = home.spawn_serve();
    let daemon_dir = home.path().join("daemon");

    assert!(
        wait_until(Duration::from_secs(5), || {
            daemon_dir.join("daemon.json").is_file() && daemon_dir.join("current.json").is_file()
        }),
        "daemon did not publish its production endpoint"
    );
    let old_record = daemon_record(home.path()).expect("started daemon records its identity");
    let old_pid = old_record.pid;
    assert_eq!(old_pid, killed.pid());
    let old_locator = read_locator(&daemon_dir).expect("started daemon publishes a locator");
    let old_socket = daemon_dir.join(&old_locator.endpoint);
    assert!(old_socket.exists());

    // `Child` is the exact process created by this fixture. Reap it so the
    // recovery path observes a completed SIGKILL rather than a zombie.
    killed.kill_and_reap();
    assert!(!process_alive(old_pid));
    assert!(daemon_dir.join("daemon.json").exists());
    assert!(daemon_dir.join("current.json").exists());
    assert!(old_socket.exists());
    assert_eq!(
        connect_current(home.path()).unwrap_err().kind(),
        std::io::ErrorKind::ConnectionRefused
    );

    // These are ordinary daemon-backed CLI requests. Release both callers
    // from the barrier together so they contend at bootstrap.lock: one must
    // retire and autostart, while the other reuses that replacement. No daemon
    // lifecycle command is issued between SIGKILL and these requests.
    let start = Barrier::new(3);
    let clients = thread::scope(|scope| {
        let first = scope.spawn(|| {
            start.wait();
            run_in_production(
                &[
                    OsStr::new("session"),
                    OsStr::new("remove"),
                    OsStr::new("missing"),
                ],
                &home,
            )
        });
        let second = scope.spawn(|| {
            start.wait();
            run_in_production(
                &[
                    OsStr::new("session"),
                    OsStr::new("remove"),
                    OsStr::new("missing"),
                ],
                &home,
            )
        });

        start.wait();
        [
            first.join().expect("first ordinary client thread"),
            second.join().expect("second ordinary client thread"),
        ]
    });
    for (index, client) in clients.iter().enumerate() {
        assert_eq!(
            client.status.code(),
            Some(1),
            "client {index}: {}",
            stderr(client)
        );
        assert!(
            stderr(client).contains("session was not found"),
            "client {index}: {}",
            stderr(client)
        );
        assert!(
            !stderr(client).contains("daemon endpoint is unavailable"),
            "client {index}: {}",
            stderr(client)
        );
    }
    assert!(
        wait_until(Duration::from_secs(5), || {
            daemon_record(home.path())
                .is_some_and(|record| record != old_record && process_alive(record.pid))
                && daemon_dir.join("current.json").is_file()
        }),
        "ordinary bootstrap did not publish a replacement endpoint"
    );

    let replacement_record = daemon_record(home.path()).expect("replacement records its identity");
    let replacement_pid = replacement_record.pid;
    let replacement = read_locator(&daemon_dir).expect("replacement publishes a locator");
    assert_ne!(replacement_record, old_record);
    assert_ne!(replacement.generation, old_locator.generation);
    assert!(!old_socket.exists());
    assert!(daemon_dir.join(&replacement.endpoint).exists());

    // Both concurrent bootstraps converged on exactly one live replacement
    // owner and generation rather than launching a duplicate daemon.
    assert_eq!(daemon_pid(home.path()), Some(replacement_pid));
    assert_eq!(read_locator(&daemon_dir).unwrap(), replacement);

    let stop = run_in_production(&[OsStr::new("daemon"), OsStr::new("stop")], &home);
    assert!(stop.status.success(), "{}", stderr(&stop));
    assert!(wait_until(Duration::from_secs(5), || {
        !process_alive(replacement_pid)
            && !daemon_dir.join("daemon.json").exists()
            && !daemon_dir.join("current.json").exists()
    }));
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
        &home,
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("session was not found"),
        "daemon request error: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_daemon_running(&home);
    stop_daemon(&home);
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
            Some(spawn_fake_daemon(home.path(), home.workspace(), reply))
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
            &home,
        );
        assert_eq!(output.status.code(), Some(case.exit_code), "{}", case.name);
        assert_eq!(stdout(&output), case.stdout, "{}", case.name);
        assert_eq!(stderr(&output), case.stderr, "{}", case.name);
        if let Some(server) = server {
            server.join().unwrap();
        }
    }
}

/// A client reached from outside the daemon's workspace must be refused instead
/// of being served another workspace's sessions, scopes, and PR inventory —
/// which is what lets `session remove` tear down a worktree the caller never
/// named (#548).
#[test]
fn the_running_daemon_admits_only_clients_inside_its_own_workspace() {
    use usagi_core::infrastructure::ipc::ClientWorkspace;
    use usagi_core::usecase::client::{ClientError, ClientPolicy, IpcClient};

    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let _daemon = home.spawn_serve();
    let daemon_dir = home.path().join("daemon");
    assert!(
        wait_until(Duration::from_secs(15), || {
            daemon_dir.join("daemon.json").is_file()
                && daemon_dir.join("current.json").is_file()
                // The lifecycle document lands in the adopted workspace's state
                // subtree, so its presence is what proves the daemon opened the
                // workspace rather than only publishing an endpoint.
                && daemon_fixture::lifecycle_state_path(home.path()).is_file()
        }),
        "daemon did not publish its production endpoint"
    );

    let connect = |workspace: ClientWorkspace| {
        IpcClient::connect(
            connect_current(home.path()).expect("the published endpoint is connectable"),
            "workspace-fence-e2e".to_owned(),
            usagi_core::domain::id::OperationId::new().to_string(),
            ClientPolicy::cli(),
            shipping_build_identity(),
            workspace,
        )
    };

    // The daemon fenced the fixture workspace at startup, so a client working in
    // it — here through the session-worktree spelling below the root — is
    // admitted exactly as before.
    let served = daemon_fixture::client_workspace(&home.production_data_dir());
    let ClientWorkspace::Bound { root: served_root } = served.clone() else {
        panic!("the fixture declares a bound workspace");
    };
    assert!(connect(served).is_ok());
    assert!(
        connect(ClientWorkspace::Bound {
            root: format!("{served_root}/.usagi/sessions/fixture"),
        })
        .is_ok()
    );

    // A sibling workspace is refused with a typed error that names the workspace
    // this daemon does serve, and a client that declares nothing is refused for
    // the same reason.
    let other = tempfile::tempdir_in("/tmp").unwrap();
    let outside = usagi_core::infrastructure::paths::wire_workspace_root(
        usagi_core::infrastructure::paths::canonical_workspace_root(other.path()).unwrap(),
    );
    let refused = connect(ClientWorkspace::Bound { root: outside })
        .err()
        .expect("a foreign workspace must not be admitted");
    let ClientError::Protocol(refusal) = refused else {
        panic!("the refusal must be a typed protocol error: {refused}");
    };
    assert_eq!(refusal.code, ErrorCode::PermissionDenied);
    assert_eq!(refusal.error_id, "workspace-mismatch");
    assert!(refusal.message.contains(&served_root), "{refusal:?}");

    // The daemon keeps serving its own workspace after the refusal.
    assert!(
        connect(daemon_fixture::client_workspace(
            &home.production_data_dir()
        ))
        .is_ok()
    );
}

/// One daemon holds several workspaces at once: selecting a workspace it has not
/// opened yet adopts it, and each adopted workspace keeps its own lifecycle
/// document. A workspace another process already fences is refused on its own,
/// without disturbing the ones this daemon holds (#710).
#[test]
fn one_daemon_adopts_every_selected_workspace_and_refuses_only_the_fenced_one() {
    use usagi_core::infrastructure::ipc::ClientWorkspace;
    use usagi_core::usecase::client::{ClientError, ClientPolicy, IpcClient};

    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let _daemon = home.spawn_serve();
    let daemon_dir = home.path().join("daemon");
    assert!(
        wait_until(Duration::from_secs(15), || {
            daemon_dir.join("daemon.json").is_file()
                && daemon_dir.join("current.json").is_file()
                && daemon_fixture::lifecycle_state_path(home.path()).is_file()
        }),
        "daemon did not publish its production endpoint"
    );

    let connect = |workspace: ClientWorkspace| {
        IpcClient::connect(
            connect_current(home.path()).expect("the published endpoint is connectable"),
            "multi-workspace-e2e".to_owned(),
            usagi_core::domain::id::OperationId::new().to_string(),
            ClientPolicy::cli(),
            shipping_build_identity(),
            workspace,
        )
    };
    let selected = |root: &Path| ClientWorkspace::Selected {
        root: usagi_core::infrastructure::paths::wire_workspace_root(
            usagi_core::infrastructure::paths::canonical_workspace_root(root)
                .expect("the fixture workspace resolves"),
        ),
    };

    // A workspace this daemon has never seen is adopted by selecting it.
    let second = daemon_fixture::short_dir("usagi-second-");
    assert!(connect(selected(second.path())).is_ok());

    // A workspace another process fences is refused alone: the refusal names it,
    // and the workspaces this daemon holds keep answering.
    let fenced = daemon_fixture::short_dir("usagi-fenced-");
    let fenced_root = usagi_core::infrastructure::paths::canonical_workspace_root(fenced.path())
        .expect("the fenced workspace resolves");
    let held = hold_workspace_fence(&fenced_root);
    let refused = connect(selected(fenced.path()))
        .err()
        .expect("a workspace another daemon owns must not be adopted");
    let ClientError::Protocol(refusal) = refused else {
        panic!("the refusal must be a typed protocol error: {refused}");
    };
    assert_eq!(refusal.error_id, "workspace-mismatch");
    assert!(
        refusal.message.contains("already owns this workspace"),
        "{refusal:?}"
    );
    drop(held);
    assert!(connect(selected(second.path())).is_ok());

    // Each adopted workspace has its own state subtree, so neither is described
    // by the other's lifecycle document.
    let adopted = usagi_core::infrastructure::workspace_state::adopted(&home.path().join("daemon"))
        .expect("the adopted workspaces are readable")
        .into_iter()
        .map(|state| state.root().to_path_buf())
        .collect::<Vec<_>>();
    assert!(
        adopted.contains(
            &usagi_core::infrastructure::paths::canonical_workspace_root(second.path()).unwrap()
        ),
        "{adopted:?}"
    );
    assert!(!adopted.contains(&fenced_root), "{adopted:?}");
    stop_daemon(&home);
}

/// Hold a workspace's fence the way a second daemon would, from a process this
/// test controls, so the daemon under test meets a genuinely owned workspace.
fn hold_workspace_fence(workspace_root: &Path) -> std::fs::File {
    use fs2::FileExt;

    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let path = usagi_core::infrastructure::paths::workspace_fence_path(workspace_root);
    let directory = path.parent().expect("the fence node has a parent");
    std::fs::create_dir_all(directory).expect("the fence directory is creatable");
    // The node is daemon-private, and a daemon refuses a fence directory that is
    // not: the point of this fixture is an owned workspace, not a broken one.
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
        .expect("the fence directory is private");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .expect("the fence node is openable");
    FileExt::try_lock_exclusive(&file).expect("the fence is free");
    file
}

/// The workspace a TUI opens — not the directory it was launched from — is what
/// the handshake declares: opening `<path>` starts the daemon *for* `<path>` and
/// keeps working from any directory (#549). One daemon now adopts a second
/// workspace on selection (#710) instead of refusing it, and each open shows its
/// own workspace: the earlier bug was rendering `<path>`'s title over the served
/// workspace's session list, which adoption must not reintroduce.
#[test]
fn opening_a_second_workspace_adopts_it_without_disturbing_the_first() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let opened = daemon_fixture::short_dir("usagi-opened-");
    let elsewhere = daemon_fixture::short_dir("usagi-elsewhere-");
    let opened_root = usagi_core::infrastructure::paths::canonical_workspace_root(opened.path())
        .expect("the opened workspace resolves");

    // No daemon yet. Opening a workspace from an unrelated directory must start a
    // daemon for the workspace being opened; a daemon bound to the launch
    // directory would then refuse the very connection that started it.
    let output = home.run_at(
        elsewhere.path(),
        &[OsStr::new("open"), opened.path().as_os_str()],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    // The project tab owns the workspace label and clips long names. Its
    // fixture-specific prefix still proves that the opened workspace, not the
    // launch directory, was rendered.
    assert!(
        stdout(&output).contains("usagi-opened-"),
        "{}",
        stdout(&output)
    );
    let recorded: serde_json::Value = serde_json::from_slice(
        &std::fs::read(daemon_fixture::lifecycle_state_path(&channel_data_dir(
            home.path(),
        )))
        .expect("the started daemon recorded its lifecycle state"),
    )
    .expect("the lifecycle state is JSON");
    assert_eq!(
        recorded["repository_root"].as_str(),
        Some(opened_root.to_str().expect("a UTF-8 fixture path")),
    );

    // A second workspace is adopted by the same daemon rather than refused, and
    // it is described as itself: its own name, its own (empty) session list.
    let elsewhere_root =
        usagi_core::infrastructure::paths::canonical_workspace_root(elsewhere.path())
            .expect("the second workspace resolves");
    let second = home.run_at(
        opened.path(),
        &[OsStr::new("open"), elsewhere.path().as_os_str()],
    );
    assert!(second.status.success(), "{}", stderr(&second));
    assert!(
        stdout(&second).contains("usagi-elsewhere-"),
        "{}",
        stdout(&second)
    );

    // Both workspaces now have their own lifecycle document, so neither is
    // described by the other's state.
    let adopted = usagi_core::infrastructure::workspace_state::adopted(
        &channel_data_dir(home.path()).join("daemon"),
    )
    .expect("the adopted workspaces are readable")
    .into_iter()
    .map(|state| state.root().to_path_buf())
    .collect::<Vec<_>>();
    assert!(adopted.contains(&opened_root), "{adopted:?}");
    assert!(adopted.contains(&elsewhere_root), "{adopted:?}");

    // The served workspace still opens, from any directory: the declaration
    // follows the selection, not the working directory.
    let output = home.run_at(
        elsewhere.path(),
        &[OsStr::new("open"), opened.path().as_os_str()],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_daemon_running(&home);
    stop_daemon(&home);
}

#[test]
fn mcp_autostarts_without_manual_daemon_start() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let mut child = home
        .command(&[OsStr::new("mcp")])
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
    assert_daemon_running(&home);
    stop_daemon(&home);
}

/// `usagi mcp` は daemon client を必須とし、handshake は client の workspace を daemon の
/// trusted root と照合する（#548）。store tool の fixture は自分で作った repository を
/// workspace とするため、その root で daemon を起動しておく。以降 root・session worktree・
/// broken store のどの cwd から実行しても、同じ workspace の内側として admit される。
fn start_daemon_for(home: &DaemonHome, workspace: &Path) {
    let output = home
        .command_at(
            Channel::Local,
            workspace,
            &[OsStr::new("daemon"), OsStr::new("start")],
        )
        .output()
        .expect("usagi バイナリを起動できる");
    assert!(output.status.success(), "{}", stderr(&output));
}

#[test]
fn mcp_store_tools_round_trip_through_stdio_and_durable_files() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let (workspace, session) = linked_issue_session("e2e");
    start_daemon_for(&home, workspace.path());
    let requests = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_create\",\"arguments\":{\"title\":\"MCP durable issue\",\"priority\":\"high\",\"labels\":[\"mcp\"],\"body\":\"round trip\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_get\",\"arguments\":{\"number\":1}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_search\",\"arguments\":{\"query\":\"durable\",\"label\":\"mcp\",\"ready\":true}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"memory_save\",\"arguments\":{\"name\":\"MCP Fact\",\"title\":\"Durable fact\",\"type\":\"project\",\"body\":\"remember me\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"tools/call\",\"params\":{\"name\":\"memory_get\",\"arguments\":{\"name\":\"mcp-fact\"}}}\n",
    );
    let output = run_mcp(&home, &session, requests);
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
    stop_daemon(&home);
}

#[test]
fn mcp_store_tools_cover_prompt_update_search_and_delete_lifecycles() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let (workspace, session) = linked_issue_session("lifecycle");
    start_daemon_for(&home, workspace.path());
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
    let output = run_mcp(&home, &session, requests);
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
    let missing = run_mcp(&home, &session, missing_requests);
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
    let broken = run_mcp(&home, &broken_session, broken_requests);
    for response in mcp_responses(&broken) {
        assert_eq!(response["error"]["code"], -32603);
    }

    let root_requests = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":26,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_create\",\"arguments\":{\"title\":\"refused\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":27,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_update\",\"arguments\":{\"number\":1}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":28,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_delete\",\"arguments\":{\"number\":1}}}\n",
    );
    let refused = run_mcp(&home, workspace.path(), root_requests);
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
    stop_daemon(&home);
}

#[test]
fn config_entry_renders_the_config_screen() {
    // `usagi config` は Config 画面を選ぶ。stdout が tty でないため、合成ルートは対話ループの
    // 代わりに Config の 1 フレームを描いて返す。Config 自体は workspace registry を使わない
    // ため、registry が壊れていても起動できる。
    let home = short_home();
    ensure_private_dir_all(&channel_data_dir(home.path())).unwrap();
    std::fs::write(
        channel_data_dir(home.path()).join("workspaces.json"),
        "{ broken",
    )
    .unwrap();
    let output = run_with_home(&[OsStr::new("config")], &home);
    assert!(output.status.success());
    let out = stdout(&output);
    assert!(out.contains("Config"));
    assert!(out.contains("Global"));
    assert!(out.contains("Theme") && out.contains("system"));
    assert!(out.contains("Workspace init"));
    assert!(out.contains("Agent"));
    assert!(out.contains("OpenAI") || out.contains("none"));
    assert!(!out.contains("Scope:"));
    assert!(out.contains("Esc: back"));
    assert!(output.stderr.is_empty());
    let status = home.run(&[OsStr::new("daemon"), OsStr::new("status")]);
    assert!(status.status.success());
    assert!(stdout(&status).contains("daemon not running"));
    stop_daemon(&home);
}

#[test]
fn config_first_boot_with_restrictive_umask_preserves_ordinary_daemon_bootstrap() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let mut config = home.command(&[OsStr::new("config")]);
    config.stdin(Stdio::null());
    // The coverage runtime creates its raw profile lazily. Pre-create this
    // child's unique profile before applying umask 0777 so llvm-profdata can
    // still read and merge the completed measurement.
    precreate_restrictive_umask_coverage_profile(&mut config);
    // SAFETY: the child has not started any threads; only its inherited umask
    // is changed before exec to exercise the first-use creation boundary.
    unsafe {
        config.pre_exec(|| {
            libc::umask(0o777);
            Ok(())
        });
    }
    let configured = config.output().expect("config first boot starts");
    assert!(configured.status.success(), "{}", stderr(&configured));
    assert_eq!(
        std::fs::metadata(channel_data_dir(home.path()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );

    let ordinary = run_with_home(
        &[
            OsStr::new("session"),
            OsStr::new("remove"),
            OsStr::new("missing"),
        ],
        &home,
    );
    assert_eq!(ordinary.status.code(), Some(1));
    assert!(stderr(&ordinary).contains("session was not found"));
    assert_daemon_running(&home);
    stop_daemon(&home);
}

#[test]
fn doctor_reports_real_diagnostics() {
    let home = short_home();
    let output = run_with_home(&[OsStr::new("doctor")], &home);
    assert!(output.status.success());
    let out = stdout(&output);
    assert!(out.contains(": doctor\n"));
    assert!(out.contains("[ok] Git: git version"));
    assert!(out.contains("Claude CLI:"));
    assert!(out.contains("OpenAI CLI:"));
    assert!(out.contains("Sakana AI CLI:"));
    assert!(out.contains("[ok] Settings: settings storage is readable"));
    assert!(out.contains("[ok] Daemon: daemon is reachable"));
    assert!(out.contains("result: healthy"));
    assert!(output.stderr.is_empty());
    stop_daemon(&home);
}

#[test]
fn open_registers_and_renders_an_explicit_or_current_workspace() {
    let home = short_home();
    let roots = tempfile::tempdir().unwrap();
    let explicit = roots.path().join("explicit-workspace");
    std::fs::create_dir(&explicit).unwrap();

    let output = run_with_home(&[OsStr::new("open"), explicit.as_os_str()], &home);
    assert!(output.status.success());
    let out = stdout(&output);
    assert!(out.contains("explicit-workspace"));
    assert!(out.contains("+ new session"));
    assert!(!out.contains("workspace main"));
    assert!(!out.contains("workspace TUI ("));
    assert_eq!(
        WorkspaceSettingsStore::new_for_mode(&explicit, RuntimeMode::Local)
            .load()
            .unwrap(),
        LocalSettings::from(&Settings::default())
    );

    // Agent / Issue / Memory are copied only when the workspace is first
    // registered. Later changes to Global workspace defaults do not rewrite it.
    Storage::new(channel_data_dir(home.path()))
        .save_settings(&Settings {
            default_model: DefaultModel::Claude,
            issue_enabled: false,
            memory_enabled: false,
            ..Settings::default()
        })
        .unwrap();
    let reopened = run_with_home(&[OsStr::new("open"), explicit.as_os_str()], &home);
    assert!(reopened.status.success());
    assert_eq!(
        WorkspaceSettingsStore::new_for_mode(&explicit, RuntimeMode::Local)
            .load()
            .unwrap(),
        LocalSettings::from(&Settings::default())
    );

    // 非 tty でも open は registry へ登録し、続く hop の Recent に現れる。
    let registry =
        std::fs::read_to_string(channel_data_dir(home.path()).join("workspaces.json")).unwrap();
    assert!(registry.contains("explicit-workspace"));
    let output = run_with_home(&[OsStr::new("hop")], &home);
    assert!(output.status.success());
    let out = stdout(&output);
    assert!(out.contains("Recent"));
    assert!(out.contains("explicit-workspace"));

    stop_daemon(&home);

    // 引数なしの open はカレントディレクトリを開く。session 一覧は daemon が返すため、
    // その workspace を所有する daemon が必要である（handshake の workspace fence。#548）。
    // ここでは cwd を workspace とする別 home を使い、その daemon を autostart させる。
    let current_home = short_home();
    let current = roots.path().join("current-workspace");
    std::fs::create_dir(&current).unwrap();
    let output = current_home.run_at(&current, &[OsStr::new("open")]);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("current-workspace"));
    assert!(out.contains("+ new session"));
    assert!(!out.contains("workspace main"));
    stop_daemon(&current_home);
}

#[test]
fn open_rejects_a_missing_or_non_directory_workspace_path() {
    let home = short_home();
    let missing = home.path().join("missing-workspace");
    let file = home.path().join("not-a-directory");
    std::fs::write(&file, "not a workspace").unwrap();

    for path in [&missing, &file] {
        let output = run_with_home(&[OsStr::new("open"), path.as_os_str()], &home);
        assert!(!output.status.success(), "path={}", path.display());
        assert!(!output.stderr.is_empty(), "path={}", path.display());
    }
}

#[test]
fn clap_errors_do_not_launch_a_tui() {
    let home = short_home();
    for args in [
        &[OsStr::new("hop"), OsStr::new("extra")][..],
        &[OsStr::new("config"), OsStr::new("extra")][..],
        &[OsStr::new("open"), OsStr::new("one"), OsStr::new("two")][..],
    ] {
        let output = run(&home, args);
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
            let args = case
                .args
                .iter()
                .copied()
                .map(OsStr::new)
                .collect::<Vec<_>>();
            let output = home
                .command(&args)
                .stdin(Stdio::null())
                .output()
                .expect("usagi バイナリを起動できる");
            let created_channel_data = channel_data_dir(home.path()).exists();
            if created_channel_data {
                stop_daemon(&home);
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

/// A workspace whose path is not UTF-8 cannot be served, so it cannot be opened.
///
/// The daemon's own authority record (`sessions.json`) and the workspace registry
/// are JSON, so such a root cannot be written down: nothing can own the workspace,
/// and the [workspace fence](../document/04-ipc.md) has no root to compare. Before
/// the fence declared the *opened* workspace this path silently rendered the
/// non-UTF-8 workspace's title over the session list of whatever workspace the
/// daemon did own (#549). It is now refused, before any daemon is started.
#[cfg(unix)]
#[test]
fn open_refuses_a_non_utf8_workspace_path_it_cannot_serve() {
    use std::os::unix::ffi::OsStringExt;

    let home = short_home();
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
    let output = run_with_home(&[OsStr::new("open"), path.as_os_str()], &home);

    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("not valid UTF-8"),
        "{}",
        stderr(&output)
    );
    // No workspace screen is rendered, so no other workspace's session list can
    // appear under this path's name.
    assert!(!stdout(&output).contains("main"), "{}", stdout(&output));
    // 壊れた registry を永続化しない。JSON の path は UTF-8 string である。
    assert!(
        !channel_data_dir(home.path())
            .join("workspaces.json")
            .exists()
    );
    // 名指せない workspace のために daemon を起動もしない。
    assert!(
        !channel_data_dir(home.path())
            .join("daemon/daemon.json")
            .exists()
    );
}

#[cfg(unix)]
#[test]
fn open_validates_non_utf8_workspace_paths() {
    use std::os::unix::ffi::OsStringExt;

    let home = short_home();
    let roots = tempfile::tempdir().unwrap();

    let missing_name = std::ffi::OsString::from_vec(b"missing-\xff".to_vec());
    let missing = roots.path().join(missing_name);
    let output = run_with_home(&[OsStr::new("open"), missing.as_os_str()], &home);
    assert!(!output.status.success());
    assert!(!output.stderr.is_empty());

    // 相対の非 UTF-8 path は絶対 path へ解決できるが、解決できても serve できないので
    // 開けない（解決不能な場合と同じく失敗し、理由を出す）。
    let relative = std::ffi::OsString::from_vec(b"relative-\xff".to_vec());
    let absolute_relative = roots.path().join(&relative);
    let relative_fixture_exists = std::fs::create_dir(&absolute_relative).is_ok();
    let output = home.run_at(roots.path(), &[OsStr::new("open"), relative.as_os_str()]);
    assert!(!output.status.success());
    assert!(!output.stderr.is_empty());
    if relative_fixture_exists {
        assert!(
            stderr(&output).contains("not valid UTF-8"),
            "{}",
            stderr(&output)
        );
    }

    // 非 UTF-8 filename を扱える filesystem では、通常 file も directory と誤認しない。
    let file_name = std::ffi::OsString::from_vec(b"file-\xff".to_vec());
    let file = roots.path().join(file_name);
    match std::fs::write(&file, "not a workspace") {
        Ok(()) => {
            let output = run_with_home(&[OsStr::new("open"), file.as_os_str()], &home);
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

/// With one generation published — every build that cannot yet roll over — owner
/// generation routing resolves to exactly the endpoint the client has always
/// used, and an owner the records do not name is refused rather than served by
/// the active daemon.
///
/// This is the regression fence for wiring the shipping client onto
/// `owner_routing`: the routing layer may only start to matter once a second
/// generation exists, so here it must be observationally identical to
/// `connect_current` ([4. IPC](../document/04-ipc.md#owner-generation-routing)).
#[test]
fn one_published_generation_routes_to_the_same_endpoint_and_refuses_an_unknown_owner() {
    use usagi_core::infrastructure::ipc::GenerationRole;
    use usagi_core::usecase::client::{ClientPolicy, IpcClient};
    use usagi_core::usecase::owner_routing::{RouteCache, RouteTarget};
    use usagi_daemon::infrastructure::generation_registry::TrustedGenerationDirectory;
    use usagi_daemon::infrastructure::unix_transport::connect_generation;

    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let _daemon = home.spawn_serve();
    let daemon_dir = home.path().join("daemon");
    assert!(
        wait_until(Duration::from_secs(15), || {
            daemon_dir.join("daemon.json").is_file() && daemon_dir.join("current.json").is_file()
        }),
        "daemon did not publish its production endpoint"
    );

    let locator = read_locator(&daemon_dir).expect("the published locator is readable");
    let mut cache = RouteCache::new(TrustedGenerationDirectory::new(home.path()));

    // A daemon that never rolled over publishes no `generations.json`, so the
    // current locator is the whole authority: one active generation, and control
    // work, owner-addressed work and the scope fan-out all land on it.
    let every = cache
        .every_generation()
        .expect("the published generation is addressable");
    assert_eq!(every.len(), 1, "one daemon publishes one generation");
    assert_eq!(every[0].role, GenerationRole::Active);
    assert_eq!(every[0].generation.as_str(), locator.generation.0);
    assert_eq!(every[0].endpoint, locator.endpoint);
    let owner = every[0].generation;
    assert_eq!(
        cache.resolve(&RouteTarget::ActiveControl).unwrap(),
        usagi_core::usecase::owner_routing::RouteResolution::Single(every[0].clone())
    );
    assert_eq!(&cache.owner(owner).unwrap(), &every[0]);

    // The resolved endpoint is the same socket `connect_current` reaches, and it
    // answers the same handshake with the same generation.
    let connect = |stream| {
        IpcClient::connect(
            stream,
            "owner-routing-e2e".to_owned(),
            usagi_core::domain::id::OperationId::new().to_string(),
            ClientPolicy::cli(),
            shipping_build_identity(),
            daemon_fixture::client_workspace(&home.production_data_dir()),
        )
        .expect("the published endpoint completes the handshake")
    };
    let routed = connect(
        connect_generation(home.path(), &every[0]).expect("the resolved endpoint is connectable"),
    );
    let current = connect(connect_current(home.path()).expect("current is connectable"));
    assert_eq!(routed.daemon_generation(), current.daemon_generation());
    assert_eq!(routed.daemon_generation().0, locator.generation.0);
    assert_eq!(routed.server_build(), current.server_build());

    // An owner the daemon-written records do not name is a typed stale target.
    // Answering it with the active endpoint would hand back a daemon that owns a
    // different set of PTYs entirely.
    let forged = usagi_core::domain::id::DaemonGeneration::new();
    let refusal = cache.owner(forged).expect_err("a forged owner is refused");
    assert_eq!(
        refusal,
        usagi_core::usecase::owner_routing::RoutingError::UnknownGeneration(forged)
    );
    assert_eq!(
        refusal.to_client_error().code(),
        ErrorCode::StaleTarget,
        "an unaddressable owner is stale, not merely unavailable"
    );
    // The refusal did not disturb the generation that is addressable.
    assert_eq!(&cache.owner(owner).unwrap(), &every[0]);
}

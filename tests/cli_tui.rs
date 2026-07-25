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

use usagi_core::domain::settings::{DefaultModel, LocalSettings, Settings};
use usagi_core::infrastructure::ipc::{
    BuildIdentity, DaemonGeneration, Envelope, EnvelopeKind, ErrorCode, OperationId, ProtocolError,
    ResponseOutcome, read_json_frame, write_json_frame,
};
use usagi_core::infrastructure::store::settings::WorkspaceSettingsStore;
use usagi_core::infrastructure::store::workspace::Storage;
use usagi_daemon::infrastructure::unix_transport::{
    EndpointLocator, EndpointState, SecureUnixListener, connect_current, ensure_private_dir_all,
    read_locator,
};

/// 起動する usagi プロセスはすべてこの fixture 経由にする。daemon の workspace root は
/// 起動時 cwd で決まるため、cwd を fixture へ固定して開発者のチェックアウトを掴ませない。
#[path = "support/daemon.rs"]
mod daemon_fixture;

use daemon_fixture::{Channel, DaemonHome};

/// Daemon lifecycle tests spawn the same test binary as a background daemon.
/// Serialize those starts so parallel integration tests cannot race its process
/// discovery and readiness publication on a loaded CI runner.
static DAEMON_LIFECYCLE_LOCK: Mutex<()> = Mutex::new(());

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
    usagi_core::infrastructure::paths::channel_data_dir(home)
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
        &std::fs::read(home.data_dir().join("daemon/sessions.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        std::fs::canonicalize(state["repository_root"].as_str().unwrap()).unwrap(),
        std::fs::canonicalize(home.workspace()).unwrap()
    );
    stop_daemon(&home);
}

#[test]
fn explicit_artifact_replacement_coalesces_without_stopping_the_running_daemon() {
    let _guard = DAEMON_LIFECYCLE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = short_home();
    let cleanup = home.spawn_serve();
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
    let second = run_in_production(&[OsStr::new("daemon"), OsStr::new("replace")], &home);
    assert!(first.status.success(), "{}", stderr(&first));
    assert!(second.status.success(), "{}", stderr(&second));
    assert_eq!(stdout(&first), stdout(&second));
    assert!(stdout(&first).contains("operation build-rollover-v1-"));
    assert_eq!(daemon_pid(home.path()), Some(old_pid));
    assert!(process_alive(old_pid));
    assert_eq!(read_locator(&daemon_dir).unwrap(), old_locator);
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
                && daemon_dir.join("sessions.json").is_file()
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
fn other_entries_route_to_their_banner_screens() {
    // 対話ループ未接続の画面（Doctor）は暫定バナー。
    let home = short_home();
    let output = run_with_home(&[OsStr::new("doctor")], &home);
    assert!(output.status.success());
    assert!(stdout(&output).contains("doctor TUI"));
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
    assert!(out.contains("main"));
    assert!(!out.contains("workspace TUI ("));
    assert_eq!(
        WorkspaceSettingsStore::new(&explicit).load().unwrap(),
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
        WorkspaceSettingsStore::new(&explicit).load().unwrap(),
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
    assert!(out.contains("main"));
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

#[cfg(unix)]
#[test]
fn open_accepts_an_existing_non_utf8_workspace_path_when_supported() {
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

    let home = short_home();
    let roots = tempfile::tempdir().unwrap();

    let missing_name = std::ffi::OsString::from_vec(b"missing-\xff".to_vec());
    let missing = roots.path().join(missing_name);
    let output = run_with_home(&[OsStr::new("open"), missing.as_os_str()], &home);
    assert!(!output.status.success());
    assert!(!output.stderr.is_empty());

    // 相対の非 UTF-8 path も、filesystem が扱える場合は絶対 path へ解決して開ける。
    let relative = std::ffi::OsString::from_vec(b"relative-\xff".to_vec());
    let absolute_relative = roots.path().join(&relative);
    let relative_fixture_exists = std::fs::create_dir(&absolute_relative).is_ok();
    let output = home.run_at(roots.path(), &[OsStr::new("open"), relative.as_os_str()]);
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

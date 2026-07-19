//! 配布バイナリの CLI 解析から TUI 起動画面までを通す結合テスト。

use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;

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
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("session was not found"),
        "daemon request error: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_daemon_running(home.path());
    stop_daemon(home.path());
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
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
        .expect("MCP initialize を書き込める");
    let output = child.wait_with_output().expect("MCP の終了を待てる");
    assert!(output.status.success());
    assert!(stdout(&output).contains("\"serverInfo\""));
    assert_daemon_running(home.path());
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
    assert!(out.contains("Scope: [Global]"));
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

//! 実 PTY 上で合成ルートの raw mode / 代替スクリーン lifetime を通す結合テスト。

#![cfg(unix)]

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use usagi_core::domain::agent::AgentProfileId;
use usagi_core::domain::agent_tab_intent::{AgentTabIntent, AgentTabIntentMutation};
use usagi_core::domain::id::{OperationId, TerminalRef, WorkspaceId};
use usagi_core::domain::settings::{LocalSettings, ModalSelectionMode};
use usagi_core::infrastructure::paths::channel_data_dir;
use usagi_core::infrastructure::store::agent_tab_intent::AgentTabIntentStore;
use usagi_core::infrastructure::store::settings::WorkspaceSettingsStore;
use usagi_core::usecase::client::{
    AgentLaunchIntent, ClientPolicy, DaemonClient, DaemonReply, DaemonRequest, IpcClient,
};
use usagi_daemon::infrastructure::unix_transport::connect_current;

/// 100×24 の PTY master/slave pair を開く。
fn open_pty() -> io::Result<(File, File)> {
    let mut master_fd = -1;
    let mut slave_fd = -1;
    let mut size = libc::winsize {
        ws_row: 24,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: output pointers refer to writable local integers, `size` is initialized, and the
    // optional terminal-name / termios pointers are null. A successful call returns two owned fds.
    let result = unsafe {
        libc::openpty(
            &raw mut master_fd,
            &raw mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut size,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `openpty` succeeded and transferred two distinct, valid descriptors to this caller.
    let pair = unsafe { (File::from_raw_fd(master_fd), File::from_raw_fd(slave_fd)) };
    Ok(pair)
}

fn terminal_attributes(terminal: &File) -> io::Result<libc::termios> {
    let mut attributes = std::mem::MaybeUninit::uninit();
    // SAFETY: `attributes` points to writable storage for one termios value and `terminal` owns a
    // live PTY slave descriptor for the duration of the call.
    if unsafe { libc::tcgetattr(terminal.as_raw_fd(), attributes.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful `tcgetattr` initialized every field of `attributes`.
    Ok(unsafe { attributes.assume_init() })
}

/// PTY の window size を更新して、foreground process に resize を通知する。
fn resize_pty(terminal: &File, columns: u16, rows: u16) -> io::Result<()> {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `terminal` owns the PTY master and `size` points to a fully initialized winsize.
    if unsafe { libc::ioctl(terminal.as_raw_fd(), libc::TIOCSWINSZ, &raw const size) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn read_pty(mut master: File) -> Vec<u8> {
    let mut output = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        match master.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => output.extend_from_slice(&chunk[..read]),
            // Linux PTYs report EIO, while Darwin normally reports EOF, after the final slave
            // descriptor closes. Both mean the captured stream is complete.
            Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
            Err(error) => panic!("PTY outputの読み取りに失敗: {error}"),
        }
    }
    output
}

fn read_pty_shared(mut master: File, output: &Arc<Mutex<Vec<u8>>>) {
    let mut chunk = [0_u8; 4096];
    loop {
        match master.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => output.lock().unwrap().extend_from_slice(&chunk[..read]),
            Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
            Err(error) => panic!("PTY outputの読み取りに失敗: {error}"),
        }
    }
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> io::Result<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            child.kill()?;
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "PTY上のusagiが終了しなかった",
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn spawn_hop(home: &std::path::Path, slave: &File) -> io::Result<Child> {
    Command::new(env!("CARGO_BIN_EXE_usagi"))
        .arg("hop")
        .env("USAGI_HOME", home)
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave.try_clone()?))
        .spawn()
}

fn spawn_hop_with_path(home: &Path, path: &str, slave: &File) -> io::Result<Child> {
    Command::new(env!("CARGO_BIN_EXE_usagi"))
        .arg("hop")
        .env("USAGI_HOME", home)
        .env("PATH", path)
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave.try_clone()?))
        .spawn()
}

fn send(master: &mut File, input: &[u8]) {
    master.write_all(input).unwrap();
    master.flush().unwrap();
}

fn short_home() -> tempfile::TempDir {
    // The daemon's generation socket is nested under USAGI_HOME. Keep this
    // real-PTY fixture within the Unix sockaddr path-length limit.
    tempfile::Builder::new()
        .prefix("usagi-")
        .tempdir_in("/tmp")
        .expect("short daemon data directory")
}

fn stop_daemon(home: &std::path::Path) {
    let output = Command::new(env!("CARGO_BIN_EXE_usagi"))
        .args(["daemon", "stop"])
        .env("USAGI_HOME", home)
        .output()
        .expect("usagi daemon stop を起動できる");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git(workspace: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .status()
        .expect("fixture git command starts");
    assert!(status.success(), "git {args:?} failed");
}

fn write_agent_fixtures(bin: &Path, codex_count: &Path, claude_count: &Path) {
    fs::create_dir_all(bin).unwrap();
    let codex = format!(
        "#!/bin/sh\nif [ \"$1\" = --version ]; then exit 0; fi\nif [ \"$1\" = login ] && [ \"$2\" = status ]; then exit 0; fi\nprintf '%s' '{{\"session_id\":\"tui-codex-lineage\",\"transcript_path\":\"/must/not/be/read.jsonl\",\"cwd\":\"/fixture\",\"hook_event_name\":\"SessionStart\",\"model\":\"fixture\"}}' | \"{}\" codex-session-capture || exit 8\nprintf 'spawn\\n' >> \"{}\"\nprintf 'codex-ready-unique\\n'\nwhile IFS= read line; do printf 'codex-input:%s\\n' \"$line\"; done\n",
        env!("CARGO_BIN_EXE_usagi"),
        codex_count.display(),
    );
    let claude = format!(
        "#!/bin/sh\nif [ \"$1\" = --version ]; then exit 0; fi\nif [ \"$1\" = auth ] && [ \"$2\" = status ]; then exit 0; fi\nprintf 'spawn\\n' >> \"{}\"\nprintf 'claude-ready-unique\\n'\nwhile IFS= read line; do printf 'claude-input:%s\\n' \"$line\"; done\n",
        claude_count.display(),
    );
    for (name, script) in [("codex", codex), ("claude", claude)] {
        let path = bin.join(name);
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn read_agent_intent(home: &Path) -> AgentTabIntent {
    let root = channel_data_dir(home).join("tui/agent-tabs");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(entries) = fs::read_dir(&root) {
            for entry in entries.flatten() {
                let path = entry.path().join("intent.json");
                if let Ok(text) = fs::read_to_string(path)
                    && let Ok(intent) = serde_json::from_str(&text)
                {
                    return intent;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "Agent tab intent was not committed"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_agent_tabs(home: &Path, expected: usize) -> AgentTabIntent {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let intent = read_agent_intent(home);
        if intent
            .targets
            .iter()
            .map(|target| target.tabs.len())
            .sum::<usize>()
            >= expected
        {
            return intent;
        }
        assert!(
            Instant::now() < deadline,
            "Agent tab intent did not reach {expected} tabs"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn daemon_pid(home: &Path) -> u64 {
    let text = fs::read_to_string(channel_data_dir(home).join("daemon/daemon.json")).unwrap();
    serde_json::from_str::<serde_json::Value>(&text).unwrap()["pid"]
        .as_u64()
        .unwrap()
}

fn launch_root_agent(home: &Path, workspace: WorkspaceId, profile: &str) -> TerminalRef {
    let data_dir = channel_data_dir(home);
    let deadline = Instant::now() + Duration::from_secs(5);
    let stream = loop {
        if let Ok(stream) = connect_current(&data_dir) {
            break stream;
        }
        assert!(Instant::now() < deadline, "daemon socket was unavailable");
        thread::sleep(Duration::from_millis(20));
    };
    let mut client = IpcClient::connect(
        stream,
        "agent-tab-intent-e2e".to_owned(),
        OperationId::new().to_string(),
        ClientPolicy::cli(),
    )
    .unwrap();
    let reply = client
        .request(DaemonRequest::Agent {
            operation_id: OperationId::new().to_string(),
            intent: AgentLaunchIntent {
                workspace,
                session: None,
                profile: Some(AgentProfileId::new(profile).unwrap()),
            },
        })
        .unwrap();
    let DaemonReply::Accepted { body, .. } = reply else {
        panic!("Agent launch was not admitted: {reply:?}");
    };
    serde_json::from_value(body["terminal"].clone()).unwrap()
}

fn wait_for_file_lines(path: &Path, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let lines = fs::read_to_string(path)
            .map(|text| text.lines().count())
            .unwrap_or_default();
        if lines >= expected {
            return;
        }
        assert!(Instant::now() < deadline, "fixture did not spawn");
        thread::sleep(Duration::from_millis(20));
    }
}

fn mutate_agent_intent(
    home: &Path,
    state: &AgentTabIntent,
    mutation: AgentTabIntentMutation,
) -> AgentTabIntent {
    AgentTabIntentStore::new(channel_data_dir(home))
        .mutate(state.workspace_id, state.revision, mutation)
        .unwrap()
        .intent
}

fn capture_occurrences(output: &Arc<Mutex<Vec<u8>>>, needle: &str) -> usize {
    String::from_utf8_lossy(&output.lock().unwrap())
        .matches(needle)
        .count()
}

fn wait_for_capture(output: &Arc<Mutex<Vec<u8>>>, needle: &str, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while capture_occurrences(output, needle) < expected {
        if Instant::now() >= deadline {
            let tail = {
                let captured = output.lock().unwrap();
                let tail_start = captured.len().saturating_sub(8_000);
                String::from_utf8_lossy(&captured[tail_start..]).into_owned()
            };
            panic!("timed out waiting for {needle}: {tail}");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn quit_workspace(master: &mut File, child: &mut Child) -> ExitStatus {
    // Leave a live pane for Switch first; bare Ctrl-Q belongs to the PTY while
    // the live terminal owns input.
    send(master, b"\x0f\x0f");
    thread::sleep(Duration::from_millis(150));
    send(master, b"\x11");
    thread::sleep(Duration::from_millis(200));
    send(master, b"\r");
    wait_with_timeout(child, Duration::from_secs(10)).expect("TUI quits normally")
}

#[test]
fn real_pty_entry_resize_quit_and_reattach_restore_terminal() {
    let home = short_home();
    let roots = tempfile::tempdir().unwrap();
    let workspace = roots.path().join("pty-workspace");
    std::fs::create_dir(&workspace).unwrap();

    // 非対話 open も同じ本番合成ルートを通して Recent 用の registry entry を作る。
    let registered = Command::new(env!("CARGO_BIN_EXE_usagi"))
        .args(["open".as_ref(), workspace.as_os_str()])
        .env("USAGI_HOME", home.path())
        .output()
        .expect("workspaceを事前登録できる");
    assert!(registered.status.success());

    let (mut master, slave) = open_pty().unwrap();
    let attributes_before = terminal_attributes(&slave).unwrap();
    let reader_master = master.try_clone().unwrap();
    let reader = thread::spawn(move || read_pty(reader_master));

    let mut child = spawn_hop(home.path(), &slave).expect("PTY上でusagi hopを起動できる");

    // `1` は Welcome の予約 input で最初の Recent を開く。`x` は Workspace 上の
    // non-reserved input で、画面遷移や quit を起こさず次フレームだけを要求する。入力は
    // PTY の line discipline が raw mode へ切り替わる時間を確保してから送る。
    thread::sleep(Duration::from_millis(150));
    send(&mut master, b"1");
    thread::sleep(Duration::from_millis(150));
    // Resize while Home is visible. The runtime must invalidate the diff base and repaint the
    // new surface instead of leaving cells from the former 100-column frame behind.
    resize_pty(&master, 80, 20).unwrap();
    thread::sleep(Duration::from_millis(100));
    // The workspace loop observes resize on the next frame boundary. `x` is a no-op key
    // which requests that boundary without changing the visible Home state.
    send(&mut master, b"x");
    thread::sleep(Duration::from_millis(100));
    // Ctrl-Q opens the TUI-close confirmation; Enter accepts it and detaches.
    // (`q` alone is inert in the controller Home loop.) Send the two keys with a
    // settle gap so the confirmation frame renders before Enter under a slow or
    // instrumented binary.
    send(&mut master, b"\x11");
    thread::sleep(Duration::from_millis(200));
    send(&mut master, b"\r");

    let status = match wait_with_timeout(&mut child, Duration::from_secs(5)) {
        Ok(status) => status,
        Err(error) => {
            drop(slave);
            drop(master);
            let captured = reader.join().unwrap();
            panic!(
                "{error}: {}",
                String::from_utf8_lossy(&captured).replace('\u{1b}', "<ESC>")
            );
        }
    };
    let attributes_after = terminal_attributes(&slave).unwrap();

    // One client can leave and immediately attach again to the same OS terminal.  A leaked raw
    // flag, alternate screen, mouse capture, or hidden cursor would make this second entry flaky.
    assert!(status.success());
    assert_eq!(attributes_after.c_iflag, attributes_before.c_iflag);
    assert_eq!(attributes_after.c_oflag, attributes_before.c_oflag);
    assert_eq!(attributes_after.c_cflag, attributes_before.c_cflag);
    assert_eq!(attributes_after.c_lflag, attributes_before.c_lflag);
    assert_eq!(attributes_after.c_cc, attributes_before.c_cc);

    let mut reattached =
        spawn_hop(home.path(), &slave).expect("同じPTYへ再接続してhopを起動できる");
    thread::sleep(Duration::from_millis(150));
    send(&mut master, b"q");
    let reattached_status = wait_with_timeout(&mut reattached, Duration::from_secs(5)).unwrap();
    let attributes_reattached = terminal_attributes(&slave).unwrap();

    // slave をすべて閉じると reader が EOF/EIO を受け取れる。
    drop(slave);
    drop(master);
    let captured = reader.join().unwrap();
    let output = String::from_utf8_lossy(&captured);

    assert!(status.success(), "PTY output: {output}");
    assert!(reattached_status.success(), "PTY output: {output}");
    assert!(output.contains("Recent"), "PTY output: {output}");
    assert!(output.contains("pty-workspace"), "PTY output: {output}");
    assert!(output.contains("main"), "PTY output: {output}");
    assert!(output.contains("\u{1b}[?1049h"), "PTY output: {output}");
    assert!(output.contains("\u{1b}[?1049l"), "PTY output: {output}");
    assert!(output.contains("\u{1b}[?25l"), "PTY output: {output}");
    assert!(output.contains("\u{1b}[?25h"), "PTY output: {output}");
    assert!(output.contains("\u{1b}[?1000h"), "PTY output: {output}");
    assert!(output.contains("\u{1b}[?1000l"), "PTY output: {output}");
    assert!(
        output.matches("\u{1b}[?1049h").count() >= 2,
        "both entries must use the alternate screen: {output}"
    );
    assert!(
        output.matches("\u{1b}[?1049l").count() >= 2,
        "both exits must restore the primary screen: {output}"
    );
    assert!(
        output.matches("\u{1b}[2J").count() >= 2,
        "the initial and resized surfaces must both be cleared: {output}"
    );

    assert_eq!(attributes_reattached.c_iflag, attributes_before.c_iflag);
    assert_eq!(attributes_reattached.c_oflag, attributes_before.c_oflag);
    assert_eq!(attributes_reattached.c_cflag, attributes_before.c_cflag);
    assert_eq!(attributes_reattached.c_lflag, attributes_before.c_lflag);
    assert_eq!(attributes_reattached.c_cc, attributes_before.c_cc);
    stop_daemon(home.path());
}

#[test]
#[allow(clippy::too_many_lines)] // One chronological multi-open PTY lifecycle is easier to audit intact.
fn real_pty_mixed_agents_restore_intent_dismissal_and_second_reopen_without_respawn() {
    let home = short_home();
    let workspace_root = tempfile::tempdir().unwrap();
    let workspace = workspace_root.path().join("agent-tabs-workspace");
    fs::create_dir(&workspace).unwrap();
    git(&workspace, &["init", "-q"]);
    git(
        &workspace,
        &["config", "user.email", "tui-e2e@example.test"],
    );
    git(&workspace, &["config", "user.name", "TUI E2E"]);
    fs::write(workspace.join("README.md"), "fixture\n").unwrap();
    git(&workspace, &["add", "README.md"]);
    git(&workspace, &["commit", "-qm", "fixture"]);

    let settings = WorkspaceSettingsStore::new(&workspace);
    let guard = settings.lock().unwrap();
    settings
        .save(&LocalSettings {
            modal_selection_mode: Some(ModalSelectionMode::Prompt),
            ..LocalSettings::default()
        })
        .unwrap();
    drop(guard);

    let fixtures = tempfile::tempdir().unwrap();
    let bin = fixtures.path().join("bin");
    let codex_count = fixtures.path().join("codex-count");
    let claude_count = fixtures.path().join("claude-count");
    write_agent_fixtures(&bin, &codex_count, &claude_count);
    let fixture_path = format!("{}:/usr/bin:/bin", bin.display());

    let registered = Command::new(env!("CARGO_BIN_EXE_usagi"))
        .args(["open".as_ref(), workspace.as_os_str()])
        .env("USAGI_HOME", home.path())
        .env("PATH", &fixture_path)
        .output()
        .expect("workspace registers");
    assert!(registered.status.success());

    let (mut master, slave) = open_pty().unwrap();
    let reader_master = master.try_clone().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let reader_capture = Arc::clone(&captured);
    let reader = thread::spawn(move || read_pty_shared(reader_master, &reader_capture));

    // First shipping TUI: launch Codex, prove its PTY accepts input, then quit
    // normally. Claude is launched below by a second daemon client so the next
    // TUI open also covers inventory-only deterministic append.
    let mut first = spawn_hop_with_path(home.path(), &fixture_path, &slave).unwrap();
    thread::sleep(Duration::from_millis(250));
    send(&mut master, b"1");
    thread::sleep(Duration::from_millis(250));
    send(&mut master, b"\ragent codex\r");
    wait_for_capture(&captured, "codex-ready-unique", 1);
    let _ = wait_for_agent_tabs(home.path(), 1);
    send(&mut master, b"codex-initial\r");
    wait_for_capture(&captured, "codex-input:codex-initial", 1);
    let status = quit_workspace(&mut master, &mut first);
    assert!(
        status.success(),
        "first TUI {status}: {}",
        String::from_utf8_lossy(&captured.lock().unwrap())
    );

    let first_intent = read_agent_intent(home.path());
    assert!(first_intent.dismissed.is_empty());
    assert_eq!(
        first_intent
            .targets
            .iter()
            .map(|target| target.tabs.len())
            .sum::<usize>(),
        1
    );
    let first_pid = daemon_pid(home.path());
    let _claude_terminal = launch_root_agent(home.path(), first_intent.workspace_id, "claude");
    wait_for_file_lines(&claude_count, 1);

    // Fresh open #1 restores saved Codex first and appends the inventory-only
    // Claude exactly once. Persisted mutations below use the same locked port
    // as the shipping reducer; keyboard routing has separate deterministic tests.
    let codex_reopen_count = capture_occurrences(&captured, "codex-ready-unique") + 1;
    let mut reopened = spawn_hop_with_path(home.path(), &fixture_path, &slave).unwrap();
    thread::sleep(Duration::from_millis(250));
    send(&mut master, b"1");
    wait_for_capture(&captured, "codex-ready-unique", codex_reopen_count);
    let _ = wait_for_agent_tabs(home.path(), 2);
    send(&mut master, b"\r");
    thread::sleep(Duration::from_millis(150));
    send(&mut master, b"codex-one\r");
    wait_for_capture(&captured, "codex-input:codex-one", 1);
    let status = quit_workspace(&mut master, &mut reopened);
    assert!(
        status.success(),
        "first reopen {status}: {}",
        String::from_utf8_lossy(&captured.lock().unwrap())
    );

    let observed = read_agent_intent(home.path());
    assert!(observed.dismissed.is_empty());
    let first_refs = observed
        .targets
        .iter()
        .flat_map(|target| &target.tabs)
        .map(|slot| (slot.continuation, slot.terminal.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(first_refs.len(), 2);
    let codex = first_intent.targets[0].tabs[0].continuation;
    let claude = *first_refs
        .keys()
        .find(|continuation| **continuation != codex)
        .unwrap();
    let ordered = mutate_agent_intent(
        home.path(),
        &observed,
        AgentTabIntentMutation::Reorder {
            session_id: None,
            continuations: vec![claude, codex],
        },
    );
    let selected_claude = mutate_agent_intent(
        home.path(),
        &ordered,
        AgentTabIntentMutation::Select {
            session_id: None,
            continuation: Some(claude),
        },
    );

    // Fresh open #2 proves persisted order/selection attach only Claude. Then a
    // durable close hides that lineage without touching its runtime.
    let claude_first_count = capture_occurrences(&captured, "claude-ready-unique") + 1;
    let mut second = spawn_hop_with_path(home.path(), &fixture_path, &slave).unwrap();
    thread::sleep(Duration::from_millis(250));
    send(&mut master, b"1");
    wait_for_capture(&captured, "claude-ready-unique", claude_first_count);
    send(&mut master, b"\r");
    thread::sleep(Duration::from_millis(150));
    send(&mut master, b"claude-one\r");
    wait_for_capture(&captured, "claude-input:claude-one", 1);
    let status = quit_workspace(&mut master, &mut second);
    assert!(
        status.success(),
        "second reopen {status}: {}",
        String::from_utf8_lossy(&captured.lock().unwrap())
    );
    let dismissed_claude = mutate_agent_intent(
        home.path(),
        &selected_claude,
        AgentTabIntentMutation::Dismiss {
            continuation: claude,
        },
    );
    assert_eq!(dismissed_claude.dismissed.len(), 1);

    // Fresh open #3 suppresses Claude and falls back to the surviving Codex.
    let codex_second_count = capture_occurrences(&captured, "codex-ready-unique") + 1;
    let mut third = spawn_hop_with_path(home.path(), &fixture_path, &slave).unwrap();
    thread::sleep(Duration::from_millis(250));
    send(&mut master, b"1");
    wait_for_capture(&captured, "codex-ready-unique", codex_second_count);
    send(&mut master, b"\r");
    thread::sleep(Duration::from_millis(150));
    send(&mut master, b"codex-second\r");
    wait_for_capture(&captured, "codex-input:codex-second", 1);
    let status = quit_workspace(&mut master, &mut third);
    assert!(
        status.success(),
        "second reopen {status}: {}",
        String::from_utf8_lossy(&captured.lock().unwrap())
    );

    // Explicit reopen clears only Claude's dismissal and never invokes either
    // provider. Selecting it makes the next fresh TUI attach the existing PTY.
    let reopened_claude = mutate_agent_intent(
        home.path(),
        &dismissed_claude,
        AgentTabIntentMutation::Reopen {
            continuation: claude,
        },
    );
    let selected_claude = mutate_agent_intent(
        home.path(),
        &reopened_claude,
        AgentTabIntentMutation::Select {
            session_id: None,
            continuation: Some(claude),
        },
    );
    assert!(selected_claude.dismissed.is_empty());

    let claude_reopened_count = capture_occurrences(&captured, "claude-ready-unique") + 1;
    let mut fourth = spawn_hop_with_path(home.path(), &fixture_path, &slave).unwrap();
    thread::sleep(Duration::from_millis(250));
    send(&mut master, b"1");
    wait_for_capture(&captured, "claude-ready-unique", claude_reopened_count);
    send(&mut master, b"\r");
    thread::sleep(Duration::from_millis(150));
    send(&mut master, b"claude-reopened\r");
    wait_for_capture(&captured, "claude-input:claude-reopened", 1);
    assert!(quit_workspace(&mut master, &mut fourth).success());

    // A second reopen retains output and input echo on the same generation.
    let claude_second_count = capture_occurrences(&captured, "claude-ready-unique") + 1;
    let mut fifth = spawn_hop_with_path(home.path(), &fixture_path, &slave).unwrap();
    thread::sleep(Duration::from_millis(250));
    send(&mut master, b"1");
    wait_for_capture(&captured, "claude-ready-unique", claude_second_count);
    send(&mut master, b"\r");
    thread::sleep(Duration::from_millis(150));
    send(&mut master, b"claude-second\r");
    wait_for_capture(&captured, "claude-input:claude-second", 1);
    assert!(quit_workspace(&mut master, &mut fifth).success());

    let final_intent = read_agent_intent(home.path());
    assert!(final_intent.dismissed.is_empty());
    let final_refs = final_intent
        .targets
        .iter()
        .flat_map(|target| &target.tabs)
        .map(|slot| (slot.continuation, slot.terminal.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(final_refs, first_refs, "TerminalRef/generation changed");
    assert_eq!(daemon_pid(home.path()), first_pid, "daemon PID changed");
    assert_eq!(fs::read_to_string(&codex_count).unwrap().lines().count(), 1);
    assert_eq!(
        fs::read_to_string(&claude_count).unwrap().lines().count(),
        1
    );

    drop(slave);
    drop(master);
    reader.join().unwrap();
    let captured = String::from_utf8_lossy(&captured.lock().unwrap()).into_owned();
    for expected in [
        "codex-input:codex-initial",
        "codex-input:codex-one",
        "codex-input:codex-second",
        "claude-input:claude-one",
        "claude-input:claude-reopened",
        "claude-input:claude-second",
    ] {
        assert!(
            captured.contains(expected),
            "missing {expected}: {captured}"
        );
    }
    assert!(captured.matches("codex-ready-unique").count() >= 3);
    assert!(captured.matches("claude-ready-unique").count() >= 3);
    stop_daemon(home.path());
}

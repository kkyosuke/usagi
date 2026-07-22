//! 実 PTY 上で合成ルートの raw mode / 代替スクリーン lifetime を通す結合テスト。

#![cfg(unix)]

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::ops::{Deref, DerefMut};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use usagi_core::domain::agent::AgentProfileId;
use usagi_core::domain::id::{OperationId, SessionId, TerminalRef, WorkspaceId};
use usagi_core::domain::settings::{LocalSettings, ModalSelectionMode};
use usagi_core::infrastructure::paths::channel_data_dir;
use usagi_core::infrastructure::store::settings::WorkspaceSettingsStore;
use usagi_core::usecase::client::{
    AgentLaunchIntent, ClientPolicy, DaemonClient, DaemonReply, DaemonRequest, IpcClient,
    SessionAction,
};
use usagi_daemon::infrastructure::unix_transport::{connect_current, read_locator};
use usagi_tui::usecase::application::agent_tab_intent::AgentTabIntent;
use usagi_tui::usecase::application::terminal_screen::TerminalScreen;

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

struct TuiChild(Child);

impl Deref for TuiChild {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for TuiChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for TuiChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

struct DaemonStopGuard(PathBuf);

impl DaemonStopGuard {
    fn new(home: &Path) -> Self {
        Self(home.to_owned())
    }
}

fn process_is_alive(pid: i32) -> bool {
    // SAFETY: signal 0 does not alter the target process and is used only to
    // probe the PID read from this test's unique temporary USAGI_HOME.
    unsafe { libc::kill(pid, 0) == 0 }
    || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn wait_for_process_exit(pid: i32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while process_is_alive(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
    true
}

impl Drop for DaemonStopGuard {
    fn drop(&mut self) {
        let daemon_pid = fs::read_to_string(channel_data_dir(&self.0).join("daemon/daemon.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|value| value["pid"].as_u64())
            .and_then(|pid| i32::try_from(pid).ok());
        let stopped = Command::new(env!("CARGO_BIN_EXE_usagi"))
            .args(["daemon", "stop"])
            .env("USAGI_HOME", &self.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()
            .and_then(|mut child| wait_with_timeout(&mut child, Duration::from_secs(2)).ok())
            .is_some_and(|status| status.success());
        if let Some(pid) = daemon_pid {
            if !stopped {
                // SAFETY: the PID was read from this test's unique temporary
                // USAGI_HOME. SIGTERM is a bounded cleanup fallback after the
                // shipping stop client itself exceeded its deadline.
                let _ = unsafe { libc::kill(pid, libc::SIGTERM) };
            }
            if !wait_for_process_exit(pid, Duration::from_secs(2)) {
                // SAFETY: the same test-owned daemon PID did not exit after a
                // bounded graceful-stop interval, so force cleanup before its
                // fixture directories are removed.
                let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
                let _ = wait_for_process_exit(pid, Duration::from_secs(1));
            }
        }
    }
}

fn spawn_hop(home: &Path, workspace: &Path, slave: &File) -> io::Result<TuiChild> {
    Command::new(env!("CARGO_BIN_EXE_usagi"))
        .arg("hop")
        .current_dir(workspace)
        .env("USAGI_HOME", home)
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave.try_clone()?))
        .spawn()
        .map(TuiChild)
}

fn spawn_hop_with_path(
    home: &Path,
    workspace: &Path,
    path: &str,
    slave: &File,
) -> io::Result<TuiChild> {
    Command::new(env!("CARGO_BIN_EXE_usagi"))
        .arg("hop")
        .current_dir(workspace)
        .env("USAGI_HOME", home)
        .env("PATH", path)
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave.try_clone()?))
        .spawn()
        .map(TuiChild)
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
        "#!/bin/sh\nif [ \"$1\" = --version ]; then exit 0; fi\nif [ \"$1\" = login ] && [ \"$2\" = status ]; then exit 0; fi\nprintf '%s' '{{\"session_id\":\"tui-codex-lineage\",\"transcript_path\":\"/must/not/be/read.jsonl\",\"cwd\":\"/fixture\",\"hook_event_name\":\"SessionStart\",\"model\":\"fixture\"}}' | \"{}\" codex-session-capture || exit 8\nprintf 'spawn\\n' >> \"{}\"\nprintf 'codex-ready-unique:%s\\n' \"$$\"\nwhile IFS= read line; do printf 'codex-input:%s\\n' \"$line\"; done\n",
        env!("CARGO_BIN_EXE_usagi"),
        codex_count.display(),
    );
    let claude = format!(
        "#!/bin/sh\nif [ \"$1\" = --version ]; then exit 0; fi\nif [ \"$1\" = auth ] && [ \"$2\" = status ]; then exit 0; fi\nprintf 'spawn\\n' >> \"{}\"\nprintf 'claude-ready-unique:%s\\n' \"$$\"\nwhile IFS= read line; do printf 'claude-input:%s\\n' \"$line\"; done\n",
        claude_count.display(),
    );
    for (name, script) in [("codex", codex), ("claude", claude)] {
        let path = bin.join(name);
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn write_terminal_fixture(path: &Path, count: &Path) {
    let script = format!(
        "#!/bin/sh\nprintf 'spawn\\n' >> \"{}\"\nprintf 'generic-ready-unique:%s\\n' \"$$\"\nwhile IFS= read line; do printf 'generic-input:%s\\n' \"$line\"; done\n",
        count.display()
    );
    fs::write(path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn read_agent_intent(home: &Path) -> AgentTabIntent {
    let root = channel_data_dir(home).join("tui/workspaces");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(entries) = fs::read_dir(&root) {
            for entry in entries.flatten() {
                let path = entry.path().join("agent-tabs.json");
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

fn wait_for_agent_intent(
    home: &Path,
    predicate: impl Fn(&AgentTabIntent) -> bool,
) -> AgentTabIntent {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let intent = read_agent_intent(home);
        if predicate(&intent) {
            return intent;
        }
        assert!(
            Instant::now() < deadline,
            "Agent tab intent did not reach the expected state"
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

fn daemon_generation(home: &Path) -> String {
    read_locator(&channel_data_dir(home).join("daemon"))
        .unwrap()
        .generation
        .0
}

fn agent_processes(home: &Path, expected: usize) -> Vec<(TerminalRef, u64)> {
    let path = channel_data_dir(home).join("daemon/agents.json");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_snapshot = String::new();
    loop {
        let text = fs::read_to_string(&path).unwrap_or_default();
        last_snapshot.clone_from(&text);
        let processes = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|snapshot| snapshot["records"].as_array().cloned())
            .map(|records| {
                let mut processes = records
                    .into_iter()
                    .filter_map(|record| {
                        if record["state"] != "running" {
                            return None;
                        }
                        let terminal =
                            serde_json::from_value(record["runtime"]["terminal"].clone()).ok()?;
                        let pid = record["process"]["pid"].as_u64()?;
                        process_is_alive(pid).then_some((terminal, pid))
                    })
                    .collect::<Vec<_>>();
                processes.sort_by_key(|(terminal, _)| serde_json::to_string(terminal).unwrap());
                processes
            })
            .unwrap_or_default();
        if processes.len() == expected {
            return processes;
        }
        assert!(
            Instant::now() < deadline,
            "Agent process identities did not reach exactly {expected} live entries: {last_snapshot}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn generic_terminal_process(home: &Path) -> (TerminalRef, u64) {
    let path = channel_data_dir(home).join("daemon/terminals.json");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_snapshot = String::new();
    loop {
        let text = fs::read_to_string(&path).unwrap_or_default();
        last_snapshot.clone_from(&text);
        let process = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|snapshot| snapshot["records"].as_array().cloned())
            .and_then(|records| {
                let [record] = records.as_slice() else {
                    return None;
                };
                if record["state"] != "running" {
                    return None;
                }
                let terminal = serde_json::from_value(record["terminal"].clone()).ok()?;
                let pid = record["process"]["pid"].as_u64()?;
                process_is_alive(pid).then_some((terminal, pid))
            });
        if let Some(process) = process {
            return process;
        }
        assert!(
            Instant::now() < deadline,
            "one live generic terminal process was not persisted: {last_snapshot}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn process_is_alive(pid: u64) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 checks existence/permission without delivering a signal
    // or otherwise mutating the target process.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn continuation_for(
    intent: &AgentTabIntent,
    terminal: &TerminalRef,
) -> usagi_core::domain::id::AgentContinuationRef {
    intent
        .targets
        .iter()
        .flat_map(|target| &target.tabs)
        .find(|slot| slot.terminal == *terminal)
        .expect("terminal has a durable Agent continuation")
        .continuation
}

fn daemon_client(home: &Path) -> IpcClient<std::os::unix::net::UnixStream> {
    let data_dir = channel_data_dir(home);
    let deadline = Instant::now() + Duration::from_secs(5);
    let stream = loop {
        if let Ok(stream) = connect_current(&data_dir) {
            break stream;
        }
        assert!(Instant::now() < deadline, "daemon socket was unavailable");
        thread::sleep(Duration::from_millis(20));
    };
    IpcClient::connect(
        stream,
        "agent-tab-intent-e2e".to_owned(),
        OperationId::new().to_string(),
        ClientPolicy::cli(),
    )
    .unwrap()
}

fn create_session(home: &Path, name: &str) -> (WorkspaceId, SessionId) {
    let mut client = daemon_client(home);
    let reply = client
        .request(DaemonRequest::Session {
            action: SessionAction::Create,
            operation_id: OperationId::new().to_string(),
            payload: serde_json::json!({"name": name}),
        })
        .unwrap();
    let body = match reply {
        DaemonReply::Accepted { body, .. } | DaemonReply::Ok(body) => body,
    };
    let workspace = serde_json::from_value(body["workspace_id"].clone()).unwrap();
    let session = body["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["name"] == name)
        .unwrap();
    let session = serde_json::from_value(session["session_id"].clone()).unwrap();
    (workspace, session)
}

fn launch_agent(
    home: &Path,
    workspace: WorkspaceId,
    session: Option<SessionId>,
    profile: &str,
) -> TerminalRef {
    let mut client = daemon_client(home);
    let reply = client
        .request(DaemonRequest::Agent {
            operation_id: OperationId::new().to_string(),
            intent: AgentLaunchIntent {
                workspace,
                session,
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

fn capture_len(output: &Arc<Mutex<Vec<u8>>>) -> usize {
    output.lock().unwrap().len()
}

fn screen_since(output: &Arc<Mutex<Vec<u8>>>, baseline: usize) -> Option<String> {
    const ALT_SCREEN_START: &[u8] = b"\x1b[?1049h";
    let captured = output.lock().unwrap();
    let bytes = captured.get(baseline..)?;
    if !bytes
        .windows(ALT_SCREEN_START.len())
        .any(|window| window == ALT_SCREEN_START)
    {
        return None;
    }
    let mut screen = TerminalScreen::new(24, 100);
    screen.advance(bytes);
    Some(screen.cells().join("\n"))
}

fn wait_for_screen_since(output: &Arc<Mutex<Vec<u8>>>, baseline: usize, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if screen_since(output, baseline).is_some_and(|screen| screen.contains(needle)) {
            return;
        }
        if Instant::now() >= deadline {
            // The PTY reader owns a separate clone. Recheck after observing the
            // deadline so output appended between the loop condition and this
            // branch cannot turn a successful product observation into a flaky
            // timeout.
            let screen = screen_since(output, baseline).unwrap_or_default();
            if screen.contains(needle) {
                return;
            }
            let tail = {
                let captured = output.lock().unwrap();
                let tail_start = captured.len().saturating_sub(8_000);
                String::from_utf8_lossy(&captured[tail_start..]).into_owned()
            };
            let all = String::from_utf8_lossy(&output.lock().unwrap()).into_owned();
            let input_feedback = [
                "terminal is busy; keystroke not delivered",
                "terminal session is no longer available",
                "terminal is reconnecting; input is temporarily unavailable",
                "terminal is disconnected; input is unavailable",
            ]
            .into_iter()
            .filter(|message| all.contains(message))
            .collect::<Vec<_>>();
            panic!(
                "timed out waiting for {needle}; feedback={input_feedback:?}; screen={screen:?}; raw tail={tail}"
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_screen_absent_since(output: &Arc<Mutex<Vec<u8>>>, baseline: usize, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if screen_since(output, baseline).is_some_and(|screen| !screen.contains(needle)) {
            return;
        }
        if Instant::now() >= deadline {
            let screen = screen_since(output, baseline).unwrap_or_default();
            panic!("timed out waiting for {needle} to close; screen={screen:?}");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn open_registered_workspace(master: &mut File, output: &Arc<Mutex<Vec<u8>>>, baseline: usize) {
    wait_for_screen_since(output, baseline, "Recent");
    send(master, b"1");
    wait_for_screen_since(output, baseline, "[switch]");
}

fn submit_closeup_command(
    master: &mut File,
    output: &Arc<Mutex<Vec<u8>>>,
    baseline: usize,
    command: &str,
) {
    send(master, b"\r");
    wait_for_screen_since(output, baseline, "Type a command:");
    send(master, format!("{command}\r").as_bytes());
    wait_for_screen_absent_since(output, baseline, "Type a command:");
}

fn activate_selected_live_pane(master: &mut File, output: &Arc<Mutex<Vec<u8>>>, baseline: usize) {
    send(master, b"\r");
    wait_for_screen_since(output, baseline, "[closeup]");
}

fn quit_from_switch(
    master: &mut File,
    child: &mut Child,
    output: &Arc<Mutex<Vec<u8>>>,
    baseline: usize,
) -> ExitStatus {
    wait_for_screen_since(output, baseline, "[switch]");
    send(master, b"\x11");
    wait_for_screen_since(output, baseline, "Detach from this workspace?");
    send(master, b"\r");
    wait_with_timeout(child, Duration::from_secs(10)).expect("TUI quits normally")
}

fn quit_workspace(
    master: &mut File,
    child: &mut Child,
    output: &Arc<Mutex<Vec<u8>>>,
    baseline: usize,
) -> ExitStatus {
    // Leave a live pane for Switch first; bare Ctrl-Q belongs to the PTY while
    // the live terminal owns input.
    send(master, b"\x0f\x0f");
    quit_from_switch(master, child, output, baseline)
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
        .current_dir(&workspace)
        .env("USAGI_HOME", home.path())
        .output()
        .expect("workspaceを事前登録できる");
    assert!(registered.status.success());

    let (mut master, slave) = open_pty().unwrap();
    let attributes_before = terminal_attributes(&slave).unwrap();
    let reader_master = master.try_clone().unwrap();
    let reader = thread::spawn(move || read_pty(reader_master));

    let mut child =
        spawn_hop(home.path(), &workspace, &slave).expect("PTY上でusagi hopを起動できる");

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
        spawn_hop(home.path(), &workspace, &slave).expect("同じPTYへ再接続してhopを起動できる");
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
#[allow(clippy::too_many_lines)] // The normal-exit and SIGKILL lifecycle is intentionally chronological.
fn real_pty_generic_terminal_survives_normal_quit_and_tui_sigkill_without_respawn() {
    let home = short_home();
    let workspace_root = tempfile::tempdir().unwrap();
    let workspace = workspace_root.path().join("generic-terminal-workspace");
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

    let fixture = tempfile::tempdir().unwrap();
    let shell = fixture.path().join("fixture-shell");
    let spawn_count = fixture.path().join("shell-spawn-count");
    write_terminal_fixture(&shell, &spawn_count);
    let _daemon_stop = DaemonStopGuard::new(home.path());
    let registered = Command::new(env!("CARGO_BIN_EXE_usagi"))
        .args(["open".as_ref(), workspace.as_os_str()])
        .current_dir(&workspace)
        .env("USAGI_HOME", home.path())
        .env("SHELL", &shell)
        .output()
        .expect("workspace registers with fixture login shell");
    assert!(registered.status.success());

    let (mut master, slave) = open_pty().unwrap();
    let reader_master = master.try_clone().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let reader_capture = Arc::clone(&captured);
    let reader = thread::spawn(move || read_pty_shared(reader_master, &reader_capture));

    // Launch the generic terminal through the shipping Closeup command, verify
    // live input, then perform the ordinary detach-and-quit path.
    let first_baseline = capture_len(&captured);
    let mut first = spawn_hop(home.path(), &workspace, &slave).unwrap();
    open_registered_workspace(&mut master, &captured, first_baseline);
    submit_closeup_command(&mut master, &captured, first_baseline, "terminal open");
    wait_for_screen_since(&captured, first_baseline, "generic-ready-unique:");
    let original_process = generic_terminal_process(home.path());
    wait_for_screen_since(
        &captured,
        first_baseline,
        &format!("generic-ready-unique:{}", original_process.1),
    );
    send(&mut master, b"generic-initial\r");
    wait_for_screen_since(&captured, first_baseline, "generic-input:generic-initial");
    let original_daemon = daemon_pid(home.path());
    let original_generation = daemon_generation(home.path());
    assert!(quit_workspace(&mut master, &mut first, &captured, first_baseline).success());
    assert_eq!(generic_terminal_process(home.path()), original_process);

    // A fresh shipping TUI replays the retained output from the same exact ref,
    // then accepts new input. Kill this TUI process so the daemon observes an
    // abrupt EOF rather than a Detach request.
    let killed_baseline = capture_len(&captured);
    let mut killed_tui = spawn_hop(home.path(), &workspace, &slave).unwrap();
    open_registered_workspace(&mut master, &captured, killed_baseline);
    wait_for_screen_since(&captured, killed_baseline, "generic-input:generic-initial");
    activate_selected_live_pane(&mut master, &captured, killed_baseline);
    send(&mut master, b"generic-before-kill\r");
    wait_for_screen_since(
        &captured,
        killed_baseline,
        "generic-input:generic-before-kill",
    );
    killed_tui.kill().unwrap();
    let killed = killed_tui.wait().unwrap();
    assert_eq!(killed.signal(), Some(libc::SIGKILL));
    assert_eq!(daemon_pid(home.path()), original_daemon);
    assert_eq!(daemon_generation(home.path()), original_generation);
    assert_eq!(generic_terminal_process(home.path()), original_process);
    assert_eq!(fs::read_to_string(&spawn_count).unwrap().lines().count(), 1);

    // Fresh open after abrupt EOF proves replay and bidirectional input on the
    // same child process. Quit normally so a second reopen can repeat the fence.
    let after_kill_baseline = capture_len(&captured);
    let mut after_kill = spawn_hop(home.path(), &workspace, &slave).unwrap();
    open_registered_workspace(&mut master, &captured, after_kill_baseline);
    wait_for_screen_since(
        &captured,
        after_kill_baseline,
        "generic-input:generic-before-kill",
    );
    activate_selected_live_pane(&mut master, &captured, after_kill_baseline);
    send(&mut master, b"generic-after-kill\r");
    wait_for_screen_since(
        &captured,
        after_kill_baseline,
        "generic-input:generic-after-kill",
    );
    assert!(
        quit_workspace(&mut master, &mut after_kill, &captured, after_kill_baseline,).success()
    );

    let second_reopen_baseline = capture_len(&captured);
    let mut second_reopen = spawn_hop(home.path(), &workspace, &slave).unwrap();
    open_registered_workspace(&mut master, &captured, second_reopen_baseline);
    wait_for_screen_since(
        &captured,
        second_reopen_baseline,
        "generic-input:generic-after-kill",
    );
    activate_selected_live_pane(&mut master, &captured, second_reopen_baseline);
    send(&mut master, b"generic-second-reopen\r");
    wait_for_screen_since(
        &captured,
        second_reopen_baseline,
        "generic-input:generic-second-reopen",
    );
    assert!(
        quit_workspace(
            &mut master,
            &mut second_reopen,
            &captured,
            second_reopen_baseline,
        )
        .success()
    );

    assert_eq!(daemon_pid(home.path()), original_daemon);
    assert_eq!(daemon_generation(home.path()), original_generation);
    assert_eq!(generic_terminal_process(home.path()), original_process);
    assert_eq!(fs::read_to_string(&spawn_count).unwrap().lines().count(), 1);

    drop(slave);
    drop(master);
    reader.join().unwrap();
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
    let _daemon_stop = DaemonStopGuard::new(home.path());

    let registered = Command::new(env!("CARGO_BIN_EXE_usagi"))
        .args(["open".as_ref(), workspace.as_os_str()])
        .current_dir(&workspace)
        .env("USAGI_HOME", home.path())
        .env("PATH", &fixture_path)
        .output()
        .expect("workspace registers");
    assert!(registered.status.success());
    let (workspace_id, session_id) = create_session(home.path(), "mixed-scope");

    let (mut master, slave) = open_pty().unwrap();
    let reader_master = master.try_clone().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let reader_capture = Arc::clone(&captured);
    let reader = thread::spawn(move || read_pty_shared(reader_master, &reader_capture));

    // First shipping TUI: launch root Codex, prove its PTY accepts input, then
    // quit normally. Two Claude runtimes are launched below by another real IPC
    // client so the next TUI open covers inventory-only deterministic append in
    // both root and managed-session scopes.
    let first_baseline = capture_len(&captured);
    let mut first = spawn_hop_with_path(home.path(), &workspace, &fixture_path, &slave).unwrap();
    open_registered_workspace(&mut master, &captured, first_baseline);
    submit_closeup_command(&mut master, &captured, first_baseline, "agent codex");
    wait_for_screen_since(&captured, first_baseline, "codex-ready-unique:");
    let first_intent = wait_for_agent_tabs(home.path(), 1);
    assert_eq!(first_intent.workspace_id, workspace_id);
    assert!(first_intent.dismissed.is_empty());
    assert_eq!(
        first_intent
            .targets
            .iter()
            .map(|target| target.tabs.len())
            .sum::<usize>(),
        1
    );
    let codex_terminal = first_intent.targets[0].tabs[0].terminal.clone();
    let first_pid = daemon_pid(home.path());
    let first_generation = daemon_generation(home.path());
    let initial_processes = agent_processes(home.path(), 1);
    let codex_pid = initial_processes
        .iter()
        .find(|(terminal, _)| terminal == &codex_terminal)
        .map(|(_, pid)| *pid)
        .expect("Codex TerminalRef has a live child PID");
    wait_for_screen_since(
        &captured,
        first_baseline,
        &format!("codex-ready-unique:{codex_pid}"),
    );
    send(&mut master, b"codex-initial\r");
    wait_for_screen_since(&captured, first_baseline, "codex-input:codex-initial");
    let status = quit_workspace(&mut master, &mut first, &captured, first_baseline);
    assert!(
        status.success(),
        "first TUI {status}: {}",
        String::from_utf8_lossy(&captured.lock().unwrap())
    );
    assert_eq!(daemon_pid(home.path()), first_pid);
    assert_eq!(daemon_generation(home.path()), first_generation);
    assert_eq!(agent_processes(home.path(), 1), initial_processes);
    let root_claude_terminal = launch_agent(home.path(), workspace_id, None, "claude");
    let session_claude_terminal =
        launch_agent(home.path(), workspace_id, Some(session_id), "claude");
    wait_for_file_lines(&claude_count, 2);
    let first_processes = agent_processes(home.path(), 3);
    assert!(
        initial_processes
            .iter()
            .all(|process| first_processes.contains(process))
    );
    let process_pid = |terminal: &TerminalRef| {
        first_processes
            .iter()
            .find(|(candidate, _)| candidate == terminal)
            .map(|(_, pid)| *pid)
            .expect("Agent TerminalRef has a persisted child PID")
    };
    let root_claude_ready = format!("claude-ready-unique:{}", process_pid(&root_claude_terminal));
    let session_claude_ready = format!(
        "claude-ready-unique:{}",
        process_pid(&session_claude_terminal)
    );

    // Fresh open #1 restores saved Codex first, then appends both inventory-only
    // Claude runtimes. All order, selection, and dismissal mutations below go
    // through shipping TUI key handling; the fixture never mutates intent files.
    let reopened_baseline = capture_len(&captured);
    let mut reopened = spawn_hop_with_path(home.path(), &workspace, &fixture_path, &slave).unwrap();
    open_registered_workspace(&mut master, &captured, reopened_baseline);
    wait_for_screen_since(&captured, reopened_baseline, "codex-input:codex-initial");
    let observed = wait_for_agent_tabs(home.path(), 3);
    let codex = continuation_for(&observed, &codex_terminal);
    let root_claude = continuation_for(&observed, &root_claude_terminal);
    let session_claude = continuation_for(&observed, &session_claude_terminal);
    let target_state = |intent: &AgentTabIntent, target_session| {
        let target = intent
            .targets
            .iter()
            .find(|target| target.session_id == target_session)
            .expect("Agent target remains present");
        (
            target
                .tabs
                .iter()
                .map(|slot| slot.continuation)
                .collect::<Vec<_>>(),
            target.selected,
        )
    };
    let first_refs = observed
        .targets
        .iter()
        .flat_map(|target| &target.tabs)
        .map(|slot| (slot.continuation, slot.terminal.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(first_refs.len(), 3);

    activate_selected_live_pane(&mut master, &captured, reopened_baseline);
    send(&mut master, b"codex-one\r");
    wait_for_screen_since(&captured, reopened_baseline, "codex-input:codex-one");

    // Ctrl-O ] moves the selected Codex after root Claude; Ctrl-O Ctrl-P then
    // selects root Claude. The new foreground alone attaches and receives input.
    send(&mut master, b"\x0f]");
    send(&mut master, b"\x0f\x10");
    wait_for_screen_since(&captured, reopened_baseline, &root_claude_ready);
    send(&mut master, b"claude-root-one\r");
    wait_for_screen_since(&captured, reopened_baseline, "claude-input:claude-root-one");
    let ordered = wait_for_agent_intent(home.path(), |intent| {
        intent.targets.iter().any(|target| {
            target.session_id.is_none()
                && target.selected == Some(root_claude)
                && target
                    .tabs
                    .iter()
                    .map(|slot| slot.continuation)
                    .collect::<Vec<_>>()
                    == [root_claude, codex]
        })
    });
    assert!(ordered.dismissed.is_empty());
    // Leave Codex selected in the second slot. A fresh UI must therefore
    // restore durable selection rather than falling back to the first slot.
    send(&mut master, b"\x0f\x0e");
    wait_for_screen_since(&captured, reopened_baseline, "codex-input:codex-one");
    let _ = wait_for_agent_intent(home.path(), |intent| {
        intent.targets.iter().any(|target| {
            target.session_id.is_none()
                && target.selected == Some(codex)
                && target
                    .tabs
                    .iter()
                    .map(|slot| slot.continuation)
                    .collect::<Vec<_>>()
                    == [root_claude, codex]
        })
    });

    // Switch to the managed session. Only its selected Claude attaches; closing
    // the tab writes a continuation-scoped dismissal and leaves its PTY alive.
    send(&mut master, b"\x0f\x0f");
    wait_for_screen_since(&captured, reopened_baseline, "[switch]");
    send(&mut master, b"\x1b[B\r");
    wait_for_screen_since(&captured, reopened_baseline, "[closeup]");
    wait_for_screen_since(&captured, reopened_baseline, &session_claude_ready);
    send(&mut master, b"claude-session-one\r");
    wait_for_screen_since(
        &captured,
        reopened_baseline,
        "claude-input:claude-session-one",
    );
    send(&mut master, b"\x0fx");
    let dismissed = wait_for_agent_intent(home.path(), |intent| {
        intent.dismissed.contains(&session_claude)
    });
    assert_eq!(dismissed.dismissed.len(), 1);
    wait_for_screen_since(&captured, reopened_baseline, "Type a command:");
    send(&mut master, b"\x1b");
    wait_for_screen_since(&captured, reopened_baseline, "[switch]");
    let status = quit_from_switch(&mut master, &mut reopened, &captured, reopened_baseline);
    assert!(
        status.success(),
        "normal reopen quit {status}: {}",
        String::from_utf8_lossy(&captured.lock().unwrap())
    );

    // Fresh open #2 proves persisted root order/selection by replaying Codex
    // from the second slot. Entering the empty managed-session Closeup and submitting
    // `reopen` clears its dismissal without a launch or resume request.
    let reopened_for_kill_baseline = capture_len(&captured);
    let mut reopened_for_kill =
        spawn_hop_with_path(home.path(), &workspace, &fixture_path, &slave).unwrap();
    open_registered_workspace(&mut master, &captured, reopened_for_kill_baseline);
    wait_for_screen_since(
        &captured,
        reopened_for_kill_baseline,
        "codex-input:codex-one",
    );
    // Reorder the restored selected tab once in each direction. The first
    // persisted result is possible only if the fresh UI projected the saved
    // [root Claude, Codex] order; the second restores that durable order.
    activate_selected_live_pane(&mut master, &captured, reopened_for_kill_baseline);
    send(&mut master, b"\x0f[");
    let _ = wait_for_agent_intent(home.path(), |intent| {
        intent.targets.iter().any(|target| {
            target.session_id.is_none()
                && target
                    .tabs
                    .iter()
                    .map(|slot| slot.continuation)
                    .collect::<Vec<_>>()
                    == [codex, root_claude]
                && target.selected == Some(codex)
        })
    });
    send(&mut master, b"\x0f]");
    let _ = wait_for_agent_intent(home.path(), |intent| {
        intent.targets.iter().any(|target| {
            target.session_id.is_none()
                && target.selected == Some(codex)
                && target
                    .tabs
                    .iter()
                    .map(|slot| slot.continuation)
                    .collect::<Vec<_>>()
                    == [root_claude, codex]
        })
    });
    send(&mut master, b"\x0f\x0f");
    wait_for_screen_since(&captured, reopened_for_kill_baseline, "[switch]");
    send(&mut master, b"\x1b[B\r");
    wait_for_screen_since(&captured, reopened_for_kill_baseline, "Type a command:");
    send(
        &mut master,
        format!("reopen {}\r", session_claude.as_str()).as_bytes(),
    );
    wait_for_screen_absent_since(&captured, reopened_for_kill_baseline, "Type a command:");
    wait_for_screen_since(
        &captured,
        reopened_for_kill_baseline,
        "claude-input:claude-session-one",
    );
    send(&mut master, b"claude-session-reopened\r");
    wait_for_screen_since(
        &captured,
        reopened_for_kill_baseline,
        "claude-input:claude-session-reopened",
    );
    let reopened_intent = wait_for_agent_intent(home.path(), |intent| intent.dismissed.is_empty());
    assert_eq!(
        target_state(&reopened_intent, None),
        (vec![root_claude, codex], Some(codex))
    );
    assert_eq!(
        target_state(&reopened_intent, Some(session_id)),
        (vec![session_claude], Some(session_claude))
    );
    let reopened_refs = reopened_intent
        .targets
        .iter()
        .flat_map(|target| &target.tabs)
        .map(|slot| (slot.continuation, slot.terminal.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(reopened_refs, first_refs);

    // Kill only the shipping TUI while its session Agent is foreground. The
    // daemon and every provider process must survive the abrupt client loss.
    reopened_for_kill.kill().unwrap();
    let killed = reopened_for_kill.wait().unwrap();
    assert_eq!(killed.signal(), Some(libc::SIGKILL));
    assert_eq!(daemon_pid(home.path()), first_pid);
    assert_eq!(daemon_generation(home.path()), first_generation);
    assert_eq!(agent_processes(home.path(), 3), first_processes);

    // Fresh open after SIGKILL waits for the root replay (the async restore
    // completion fence) before interacting, then switches to the session and
    // proves retained output plus new input on the same PTY.
    let after_kill_baseline = capture_len(&captured);
    let mut after_kill =
        spawn_hop_with_path(home.path(), &workspace, &fixture_path, &slave).unwrap();
    open_registered_workspace(&mut master, &captured, after_kill_baseline);
    wait_for_screen_since(&captured, after_kill_baseline, "codex-input:codex-one");
    send(&mut master, b"\x1b[B\r");
    wait_for_screen_since(&captured, after_kill_baseline, "[closeup]");
    wait_for_screen_since(
        &captured,
        after_kill_baseline,
        "claude-input:claude-session-reopened",
    );
    send(&mut master, b"claude-session-after-kill\r");
    wait_for_screen_since(
        &captured,
        after_kill_baseline,
        "claude-input:claude-session-after-kill",
    );
    assert!(
        quit_workspace(&mut master, &mut after_kill, &captured, after_kill_baseline,).success()
    );

    // A second fresh reopen retains the post-kill output and still addresses
    // the same daemon-owned terminal rather than replaying a launch intent.
    let second_reopen_baseline = capture_len(&captured);
    let mut second_reopen =
        spawn_hop_with_path(home.path(), &workspace, &fixture_path, &slave).unwrap();
    open_registered_workspace(&mut master, &captured, second_reopen_baseline);
    wait_for_screen_since(&captured, second_reopen_baseline, "codex-input:codex-one");
    send(&mut master, b"\x1b[B\r");
    wait_for_screen_since(&captured, second_reopen_baseline, "[closeup]");
    wait_for_screen_since(
        &captured,
        second_reopen_baseline,
        "claude-input:claude-session-after-kill",
    );
    send(&mut master, b"claude-session-second-reopen\r");
    wait_for_screen_since(
        &captured,
        second_reopen_baseline,
        "claude-input:claude-session-second-reopen",
    );
    assert!(
        quit_workspace(
            &mut master,
            &mut second_reopen,
            &captured,
            second_reopen_baseline,
        )
        .success()
    );

    let final_intent = read_agent_intent(home.path());
    assert!(final_intent.dismissed.is_empty());
    assert_eq!(
        target_state(&final_intent, None),
        (vec![root_claude, codex], Some(codex))
    );
    assert_eq!(
        target_state(&final_intent, Some(session_id)),
        (vec![session_claude], Some(session_claude))
    );
    let final_refs = final_intent
        .targets
        .iter()
        .flat_map(|target| &target.tabs)
        .map(|slot| (slot.continuation, slot.terminal.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(final_refs, first_refs, "TerminalRef changed");
    assert_eq!(daemon_pid(home.path()), first_pid, "daemon PID changed");
    assert_eq!(
        daemon_generation(home.path()),
        first_generation,
        "daemon generation changed"
    );
    assert_eq!(
        agent_processes(home.path(), 3),
        first_processes,
        "Agent process PID changed"
    );
    assert_eq!(fs::read_to_string(&codex_count).unwrap().lines().count(), 1);
    assert_eq!(
        fs::read_to_string(&claude_count).unwrap().lines().count(),
        2
    );

    drop(slave);
    drop(master);
    reader.join().unwrap();
}

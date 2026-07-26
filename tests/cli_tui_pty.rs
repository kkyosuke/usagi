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
use std::sync::{Arc, LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use usagi_core::domain::agent::AgentProfileId;
use usagi_core::domain::id::{OperationId, SessionId, TerminalRef, WorkspaceId};
use usagi_core::domain::settings::{ModalSelectionMode, Settings};
use usagi_core::domain::terminal_launch::{
    TerminalLaunchRequest, TerminalLaunchScope, TerminalProfileId,
};
use usagi_core::infrastructure::paths::channel_data_dir;
use usagi_core::infrastructure::store::workspace::Storage;
use usagi_core::usecase::client::{
    AgentLaunchIntent, ClientPolicy, DaemonClient, DaemonReply, DaemonRequest, IpcClient,
    SessionAction, TerminalAction, TerminalGeometry, TerminalLaunchIntent, TerminalRequest,
};
use usagi_daemon::infrastructure::unix_transport::{
    connect_current, ensure_private_dir_all, read_locator,
};
use usagi_tui::usecase::application::agent_tab_intent::AgentTabIntent;
use usagi_tui::usecase::application::terminal_screen::TerminalScreen;

/// 起動する usagi プロセスはすべてこの fixture 経由にする（cwd の fixture 固定と daemon の reap）。
#[path = "support/daemon.rs"]
mod daemon_fixture;

use daemon_fixture::{Channel, DaemonHome};

/// Claude は必ず OS sandbox launcher の中で起動するため、`bwrap` を持たない Linux CI では
/// fail-closed で起動が拒否される。この debug ビルド専用 seam は launcher と `--settings` フックの
/// live 配線をそのまま通したまま拘束だけを外し、E2E を platform 非依存にする
/// （[`usagi_core::usecase::claude_sandbox::passthrough_requested`]）。
const SANDBOX_PASSTHROUGH: &str =
    usagi_core::usecase::claude_sandbox::PASSTHROUGH_ENVIRONMENT_VARIABLE;

/// 実 PTY テストは shipping binary・daemon・fixture provider を同時に走らせるため CPU を占有する。
/// 1 binary 内で並行させると frame 待ちが product の失敗ではなく CPU 競合による timeout になるので、
/// この file のテストは直列に実行する（`tests/agent_ipc_e2e.rs` の daemon 起動 lock と同じ方針）。
static PTY_SERIAL: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn serial() -> std::sync::MutexGuard<'static, ()> {
    PTY_SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn shipping_build_identity() -> usagi_core::infrastructure::ipc::BuildIdentity {
    usagi_core::infrastructure::ipc::build_identity(
        env!("CARGO_PKG_VERSION"),
        env!("USAGI_BUILD_COMMIT"),
        env!("USAGI_BUILD_TARGET"),
        env!("USAGI_BUILD_PROFILE"),
        env!("USAGI_BUILD_SOURCE_ID"),
    )
}

/// 100×24 の PTY master/slave pair を開く。
///
/// `openpty` の返す fd は close-on-exec ではないため、この pair を明示的に CLOEXEC にする。
/// そうしないと、PTY を開いた後に起動した usagi プロセス（`hop`）だけでなく、そこから
/// bootstrap される**常駐 daemon**まで master / slave を継承してしまう。daemon はテストより
/// 長生きするので、テスト終了時に master / slave を閉じても reader が EOF を受け取れず
/// `join()` が永久に待つ。子へ渡す stdio は `try_clone` → `dup2` 経路で CLOEXEC が外れるため、
/// この設定は PTY 上の TUI 起動を妨げない。
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
    for fd in [master_fd, slave_fd] {
        // SAFETY: `openpty` succeeded, so both descriptors are valid and owned here.
        // `FD_CLOEXEC` is the only `F_SETFD` flag, so setting it clobbers nothing.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            let error = io::Error::last_os_error();
            // SAFETY: both descriptors are still owned and unclosed at this point.
            unsafe {
                libc::close(master_fd);
                libc::close(slave_fd);
            }
            return Err(error);
        }
    }
    // SAFETY: `openpty` succeeded and transferred two distinct, valid descriptors to this caller.
    let pair = unsafe { (File::from_raw_fd(master_fd), File::from_raw_fd(slave_fd)) };
    Ok(pair)
}

fn write_prompt_settings(home: &Path) {
    let data_dir = channel_data_dir(home);
    ensure_private_dir_all(&data_dir).unwrap();
    let storage = Storage::new(data_dir);
    let _guard = storage.lock().unwrap();
    storage
        .save_settings(&Settings {
            modal_selection_mode: ModalSelectionMode::Prompt,
            ..Settings::default()
        })
        .unwrap();
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

fn spawn_hop(home: &DaemonHome, workspace: &Path, slave: &File) -> io::Result<TuiChild> {
    home.command_at(Channel::Local, workspace, &["hop".as_ref()])
        .env(SANDBOX_PASSTHROUGH, "1")
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave.try_clone()?))
        .spawn()
        .map(TuiChild)
}

fn spawn_hop_with_path(
    home: &DaemonHome,
    workspace: &Path,
    path: &str,
    slave: &File,
) -> io::Result<TuiChild> {
    home.command_at(Channel::Local, workspace, &["hop".as_ref()])
        .env("PATH", path)
        .env(SANDBOX_PASSTHROUGH, "1")
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

fn short_home() -> DaemonHome {
    DaemonHome::new()
}

fn stop_daemon(home: &DaemonHome) {
    let output = home.run(&["daemon".as_ref(), "stop".as_ref()]);
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

/// provider-native な会話 ID。Codex fixture は #504 の production structured capture
/// （`SessionStart` フック）でこれを報告し、resume argv にそのまま現れる。画面へ出てはならない。
const CODEX_LINEAGE: &str = "tui-codex-lineage";
/// capture が申告する transcript / cwd。どちらも provider 由来の sensitive metadata で、
/// 画面にも log にも出てはならない。
const CODEX_TRANSCRIPT: &str = "/must/not/be/read.jsonl";
const CODEX_CAPTURED_CWD: &str = "/must/not/be/shown";

/// PATH 上に置く Codex / Claude fixture と、その観測用ファイル群。
///
/// 各 provider は spawn ごとに count へ 1 行、argv へ 1 行を追記する。したがって
/// 「child が何回起動したか」と「resume argv が exact な provider session ID を運んだか」を
/// プロセス外から観測できる。argv は**ファイルにだけ**書くので、画面へ出ていないことの
/// assertion と両立する。
struct AgentFixtures {
    bin: PathBuf,
    codex_count: PathBuf,
    codex_argv: PathBuf,
    claude_count: PathBuf,
    claude_argv: PathBuf,
}

impl AgentFixtures {
    fn new(root: &Path) -> Self {
        Self {
            bin: root.join("bin"),
            codex_count: root.join("codex-count"),
            codex_argv: root.join("codex-argv"),
            claude_count: root.join("claude-count"),
            claude_argv: root.join("claude-argv"),
        }
    }

    /// 起動する usagi プロセスへ渡す PATH。
    fn path_env(&self) -> String {
        format!("{}:/usr/bin:/bin", self.bin.display())
    }

    fn write(&self) {
        fs::create_dir_all(&self.bin).unwrap();
        // resume 起動は `resume <provider session id>` を argv に持つ。initial 起動だけが
        // production の structured capture を通す（resume で再 capture すると lineage が分岐する）。
        let codex = format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then exit 0; fi\nif [ \"$1\" = login ] && [ \"$2\" = status ]; then exit 0; fi\nprintf '%s\\n' \"$*\" >> \"{argv}\"\nresuming=false\nfor argument in \"$@\"; do if [ \"$argument\" = resume ]; then resuming=true; fi; done\nif [ \"$resuming\" = false ]; then\n  printf '%s' '{{\"session_id\":\"{lineage}\",\"transcript_path\":\"{transcript}\",\"cwd\":\"{cwd}\",\"hook_event_name\":\"SessionStart\",\"model\":\"fixture\"}}' | \"{usagi}\" codex-session-capture || exit 8\nfi\nprintf 'spawn\\n' >> \"{count}\"\nif [ \"$resuming\" = true ]; then printf 'codex-resumed-unique:%s\\n' \"$$\"; else printf 'codex-ready-unique:%s\\n' \"$$\"; fi\nwhile IFS= read line; do printf 'codex-input:%s\\n' \"$line\"; done\n",
            argv = self.codex_argv.display(),
            lineage = CODEX_LINEAGE,
            transcript = CODEX_TRANSCRIPT,
            cwd = CODEX_CAPTURED_CWD,
            usagi = env!("CARGO_BIN_EXE_usagi"),
            count = self.codex_count.display(),
        );
        // 起動 argv も 1 行として記録し、live 配線（`--settings` のフック JSON）と、Claude の
        // daemon-issued ID（initial は `--session-id`、resume は同じ ID の `--resume`）を観測する。
        let claude = format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then exit 0; fi\nif [ \"$1\" = auth ] && [ \"$2\" = status ]; then exit 0; fi\nprintf '%s\\n' \"$*\" >> \"{argv}\"\nprintf 'spawn\\n' >> \"{count}\"\nresuming=false\nfor argument in \"$@\"; do if [ \"$argument\" = --resume ]; then resuming=true; fi; done\nif [ \"$resuming\" = true ]; then printf 'claude-resumed-unique:%s\\n' \"$$\"; else printf 'claude-ready-unique:%s\\n' \"$$\"; fi\nwhile IFS= read line; do printf 'claude-input:%s\\n' \"$line\"; done\n",
            argv = self.claude_argv.display(),
            count = self.claude_count.display(),
        );
        for (name, script) in [("codex", codex), ("claude", claude)] {
            let path = self.bin.join(name);
            fs::write(&path, script).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn codex_spawns(&self) -> usize {
        spawn_count(&self.codex_count)
    }

    fn claude_spawns(&self) -> usize {
        spawn_count(&self.claude_count)
    }

    fn codex_launch_argv(&self) -> Vec<String> {
        argv_lines(&self.codex_argv)
    }

    fn claude_launch_argv(&self) -> Vec<String> {
        argv_lines(&self.claude_argv)
    }
}

/// fixture provider が今までに起動した child の数。
fn spawn_count(provider: &Path) -> usize {
    fs::read_to_string(provider)
        .map(|text| text.lines().count())
        .unwrap_or_default()
}

/// fixture provider が記録した起動 argv（1 spawn 1 行、起動順）。
fn argv_lines(provider: &Path) -> Vec<String> {
    fs::read_to_string(provider)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
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
        shipping_build_identity(),
        daemon_fixture::client_workspace(&data_dir),
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

/// `expected` 件の live generic terminal が persist されるまで待ち、その exact ref と pid を返す。
fn generic_terminal_processes(home: &Path, expected: usize) -> Vec<(TerminalRef, u64)> {
    let path = channel_data_dir(home).join("daemon/terminals.json");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_snapshot = String::new();
    loop {
        let text = fs::read_to_string(&path).unwrap_or_default();
        last_snapshot.clone_from(&text);
        let processes = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|snapshot| snapshot["records"].as_array().cloned())
            .map(|records| {
                records
                    .iter()
                    .filter(|record| record["state"] == "running")
                    .filter_map(|record| {
                        let terminal = serde_json::from_value(record["terminal"].clone()).ok()?;
                        let pid = record["process"]["pid"].as_u64()?;
                        process_is_alive(pid).then_some((terminal, pid))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if processes.len() == expected {
            return processes;
        }
        assert!(
            Instant::now() < deadline,
            "{expected} live generic terminal processes were not persisted: {last_snapshot}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// `sibling` と同じ scope に、TUI を介さない real IPC client から generic terminal を 1 本起動する。
fn launch_generic_terminal(home: &Path, sibling: &TerminalRef) -> TerminalRef {
    let mut client = daemon_client(home);
    let payload = serde_json::to_value(TerminalRequest::Launch {
        intent: TerminalLaunchIntent {
            request: TerminalLaunchRequest {
                profile_id: TerminalProfileId::new("login-shell").unwrap(),
                scope: TerminalLaunchScope {
                    workspace_id: sibling.workspace_id,
                    session_id: sibling.session_id,
                    worktree_id: sibling.worktree_id,
                },
            },
            geometry: TerminalGeometry { cols: 80, rows: 20 },
        },
    })
    .unwrap();
    let reply = client
        .request(DaemonRequest::Terminal {
            action: TerminalAction::Launch,
            payload,
        })
        .unwrap();
    let (DaemonReply::Ok(body) | DaemonReply::Accepted { body, .. }) = reply;
    serde_json::from_value(body["terminal"].clone()).unwrap()
}

/// 右ペインに現在描かれている terminal の fixture pid。selected tab の replay だけが見えるので、
/// これは foreground の process を指す。
fn displayed_terminal_pid(output: &Arc<Mutex<Vec<u8>>>, baseline: usize) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let screen = screen_since(output, baseline).unwrap_or_default();
        let pid = screen
            .split("generic-ready-unique:")
            .nth(1)
            .map(|tail| {
                tail.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
            })
            .and_then(|digits| digits.parse::<u64>().ok());
        if let Some(pid) = pid {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "no terminal replay marker was displayed; screen={screen:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
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
                "terminal stream is unavailable",
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

/// 描画された tab strip が選択中として印を付けている tab の label。
///
/// widget は選択 chip の真下だけを `▔` で埋めるので、marker の列範囲で chip 行を切ると
/// その chip だけが取れる。selection の判定を label 文字列の探索ではなく描画された marker から
/// 行うため、同じ label が複数あっても取り違えない。
fn selected_tab_label(screen: &str) -> Option<String> {
    let rows = screen
        .lines()
        .map(|row| row.chars().collect::<Vec<_>>())
        .collect::<Vec<_>>();
    for index in 1..rows.len() {
        let marker = &rows[index];
        let Some(start) = marker.iter().position(|cell| *cell == '▔') else {
            continue;
        };
        let width = marker[start..]
            .iter()
            .take_while(|cell| **cell == '▔')
            .count();
        let chips = &rows[index - 1];
        if start + width > chips.len() {
            continue;
        }
        let label = chips[start..start + width].iter().collect::<String>();
        return Some(label.trim().to_owned());
    }
    None
}

/// 描画された tab strip が持つ generic terminal chip の数。
///
/// chip 行は選択 marker (`▔`) 行の直上なので、`selected_tab_label` と同じ方法で行を特定し、
/// その行の label 出現数だけを数える。tab の identity ではなく「何枚描かれているか」だけを見る。
fn terminal_tab_count(screen: &str) -> usize {
    let rows = screen
        .lines()
        .map(|row| row.chars().collect::<Vec<_>>())
        .collect::<Vec<_>>();
    for index in 1..rows.len() {
        if !rows[index].contains(&'▔') {
            continue;
        }
        return rows[index - 1]
            .iter()
            .collect::<String>()
            .matches("Terminal")
            .count();
    }
    0
}

/// tab strip が `expected` 枚の generic terminal tab を描くまで待つ。
///
/// background exit の観測は inventory cadence（2s）＋ backoff に律速されるので、frame 単位ではなく
/// この上限で待つ。
fn wait_for_tab_count(output: &Arc<Mutex<Vec<u8>>>, baseline: usize, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let screen = screen_since(output, baseline).unwrap_or_default();
        if terminal_tab_count(&screen) == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "tab strip never settled on {expected} terminal tabs; screen={screen:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// fixture terminal の子 process を落とす。TUI は attach していないので、これは daemon 側だけで
/// 起きる exit である。
fn kill_process(pid: u64) {
    let pid = i32::try_from(pid).expect("fixture pid fits in pid_t");
    // SAFETY: `pid` is a live child of the daemon observed by this test and the
    // call only delivers a signal to it.
    let result = unsafe { libc::kill(pid, libc::SIGKILL) };
    assert_eq!(result, 0, "failed to kill the background terminal process");
}

/// `Ctrl-O Ctrl-N` の実キー入力で tab を巡回し、`label` の tab が選択されるまで待つ。
///
/// selection は描画された marker から読むので、durable な復元順に依存しない。
fn select_tab_by_label(
    master: &mut File,
    output: &Arc<Mutex<Vec<u8>>>,
    baseline: usize,
    label: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let screen = screen_since(output, baseline).unwrap_or_default();
        if selected_tab_label(&screen).as_deref() == Some(label) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "tab {label} was never selected; screen={screen:?}"
        );
        send(master, b"\x0f\x0e");
        thread::sleep(Duration::from_millis(150));
    }
}

/// `count` が `expected` に達し、そのまま留まることを確認する。
///
/// 「double click が child spawn 1 件へ収束する」のように「増えない」ことが主張の中身である
/// 場合、到達を待つだけでは 2 個目の spawn を見逃す。到達後に settle 窓を置いて再確認する。
fn assert_spawns_settle(count: &Path, expected: usize) {
    wait_for_file_lines(count, expected);
    let observed = fs::read_to_string(count)
        .map(|text| text.lines().count())
        .unwrap_or_default();
    assert_eq!(
        observed,
        expected,
        "{} spawned too many children",
        count.display()
    );
    thread::sleep(Duration::from_millis(400));
    let settled = fs::read_to_string(count)
        .map(|text| text.lines().count())
        .unwrap_or_default();
    assert_eq!(
        settled,
        expected,
        "{} spawned an extra child after settling",
        count.display()
    );
}

/// この home が記録した exact な daemon incarnation を SIGKILL する。
///
/// cold failure は `daemon stop` では代用できない（stop は live resource を retire して
/// しまう）。pid だけでなく process-start identity も突き合わせるので、pid 再利用や
/// 置き換わった daemon を撃つことはない。
fn sigkill_daemon(home: &Path) -> (u64, String) {
    let pid = daemon_pid(home);
    let generation = daemon_generation(home);
    let record = fs::read_to_string(channel_data_dir(home).join("daemon/daemon.json")).unwrap();
    let recorded =
        serde_json::from_str::<serde_json::Value>(&record).unwrap()["process_start_identity"]
            .as_str()
            .expect("daemon record carries its process-start identity")
            .to_owned();
    assert_eq!(
        recorded,
        daemon_fixture::process_start_identity(u32::try_from(pid).unwrap()),
        "the recorded daemon pid is no longer that incarnation"
    );
    // SAFETY: identity を直前に照合した、このテストの home が記録した daemon だけを撃つ。
    unsafe { libc::kill(libc::pid_t::try_from(pid).unwrap(), libc::SIGKILL) };
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_is_alive(pid) {
        assert!(Instant::now() < deadline, "the daemon survived SIGKILL");
        thread::sleep(Duration::from_millis(20));
    }
    (pid, generation)
}

/// `pids` のプロセスがすべて消えるまで待つ。
///
/// daemon を SIGKILL すると PTY master が閉じ、その子 provider は EOF で終了する。
/// 「旧 PTY が live 復元されない」ことの前提は、まず旧 child が本当に消えていることである。
fn wait_for_dead_processes(pids: &[u64]) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if pids.iter().all(|pid| !process_is_alive(*pid)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "provider children survived the cold failure: {pids:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// `baseline` 以降に PTY へ書かれた**生バイト列**に、どの秘密も現れないことを確認する。
///
/// VT parse 後の frame ではなく生ストリームを見るため、描画された画面だけでなく
/// stderr へ落ちた診断（log）も同じ 1 か所で押さえられる。
fn assert_no_sensitive_output(output: &Arc<Mutex<Vec<u8>>>, baseline: usize, secrets: &[&str]) {
    let captured = output.lock().unwrap();
    let stream = String::from_utf8_lossy(&captured[baseline.min(captured.len())..]);
    for secret in secrets {
        assert!(
            !stream.contains(secret),
            "the shipping TUI leaked {secret} to its terminal"
        );
    }
}

/// argv 1 行から Claude の daemon-issued provider session ID を取り出す。
fn claude_session_id(argv: &str, flag: &str) -> String {
    let mut arguments = argv.split_whitespace();
    while let Some(argument) = arguments.next() {
        if argument == flag {
            return arguments
                .next()
                .expect("the Claude flag carries its provider session ID")
                .to_owned();
        }
    }
    panic!("{flag} was not present in {argv}");
}

#[test]
fn real_pty_entry_resize_quit_and_reattach_restore_terminal() {
    let _serial = serial();
    let home = short_home();
    let roots = tempfile::tempdir().unwrap();
    let workspace = roots.path().join("pty-workspace");
    std::fs::create_dir(&workspace).unwrap();

    // 非対話 open も同じ本番合成ルートを通して Recent 用の registry entry を作る。
    let registered = home
        .command_at(
            Channel::Local,
            &workspace,
            &["open".as_ref(), workspace.as_os_str()],
        )
        .output()
        .expect("workspaceを事前登録できる");
    assert!(registered.status.success());

    let (mut master, slave) = open_pty().unwrap();
    let attributes_before = terminal_attributes(&slave).unwrap();
    let reader_master = master.try_clone().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let reader_capture = Arc::clone(&captured);
    let reader = thread::spawn(move || read_pty_shared(reader_master, &reader_capture));
    let baseline = capture_len(&captured);

    let mut child = spawn_hop(&home, &workspace, &slave).expect("PTY上でusagi hopを起動できる");

    // #554. Since the frame loop skips a redraw whose material is unchanged,
    // every step below waits for the *screen* rather than sleeping. Welcome is
    // the strongest place to pin it: that screen has no animation at all, so a
    // gate that swallowed an input would leave it frozen forever instead of
    // being rescued by the next tick. `1` は Welcome の予約 input で最初の
    // Recent を開く。
    wait_for_screen_since(&captured, baseline, "Recent");
    send(&mut master, b"1");
    wait_for_screen_since(&captured, baseline, "[switch]");
    // Resize while Home is visible. The runtime must invalidate the diff base and repaint the
    // new surface instead of leaving cells from the former 100-column frame behind.
    resize_pty(&master, 80, 20).unwrap();
    // The workspace loop observes resize on the next frame boundary. `x` is a no-op key
    // which requests that boundary without changing the visible Home state.
    send(&mut master, b"x");
    // Ctrl-Q opens the TUI-close confirmation; Enter accepts it and detaches.
    // (`q` alone is inert in the controller Home loop.) Waiting for the
    // confirmation instead of sleeping is what pins the keystroke's reflection.
    send(&mut master, b"\x11");
    wait_for_screen_since(&captured, baseline, "Detach from this workspace?");
    send(&mut master, b"\r");

    let status = match wait_with_timeout(&mut child, Duration::from_secs(5)) {
        Ok(status) => status,
        Err(error) => {
            drop(slave);
            drop(master);
            reader.join().unwrap();
            let captured = captured.lock().unwrap().clone();
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

    let reattach_baseline = capture_len(&captured);
    let mut reattached =
        spawn_hop(&home, &workspace, &slave).expect("同じPTYへ再接続してhopを起動できる");
    wait_for_screen_since(&captured, reattach_baseline, "Recent");
    send(&mut master, b"q");
    let reattached_status = wait_with_timeout(&mut reattached, Duration::from_secs(5)).unwrap();
    let attributes_reattached = terminal_attributes(&slave).unwrap();

    // slave をすべて閉じると reader が EOF/EIO を受け取れる。
    drop(slave);
    drop(master);
    reader.join().unwrap();
    let captured = captured.lock().unwrap().clone();
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
    stop_daemon(&home);
}

#[test]
#[allow(clippy::too_many_lines)] // The normal-exit and SIGKILL lifecycle is intentionally chronological.
fn real_pty_generic_terminal_survives_normal_quit_and_tui_sigkill_without_respawn() {
    let _serial = serial();
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

    write_prompt_settings(home.path());

    let fixture = tempfile::tempdir().unwrap();
    let shell = fixture.path().join("fixture-shell");
    let spawn_count = fixture.path().join("shell-spawn-count");
    write_terminal_fixture(&shell, &spawn_count);
    let registered = home
        .command_at(
            Channel::Local,
            &workspace,
            &["open".as_ref(), workspace.as_os_str()],
        )
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
    let mut first = spawn_hop(&home, &workspace, &slave).unwrap();
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
    let mut killed_tui = spawn_hop(&home, &workspace, &slave).unwrap();
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
    let mut after_kill = spawn_hop(&home, &workspace, &slave).unwrap();
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
    let mut second_reopen = spawn_hop(&home, &workspace, &slave).unwrap();
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

/// 実 daemon・実 PTY 2 本: detach された background tab の process が exit したとき、
/// scope inventory lane **だけ**がそれを観測して tab を閉じ、foreground pane は流れ続ける。
///
/// background tab には `Attach` も `Resume` も送らないので、観測しても process は増えない
/// （[3. TUI#背景 observation lane](../document/03-tui.md)）。
/// 応答を止めた daemon が、出荷 TUI を固めないことの product 上の bound。
///
/// attach/input lane は以前 deadline を持たない生 socket で、`Input` を書いたあと daemon が
/// 答えなければ描画スレッドが無期限に停止した（#553）。lane は request ごとに
/// `TerminalLaneBudget` で armed になったので、SIGSTOP した daemon（＝accept も応答もしないが
/// socket は生きている、hung daemon そのものの形）に対しても、live pane へのキー入力・switch
/// overlay の描画・quit がすべて wall-clock で有界に戻る。修正前はこのテストが timeout する。
#[test]
fn real_pty_hung_daemon_bounds_redraw_and_quit_with_an_attached_pane() {
    /// 全体の wall-clock 上限。frame ごとの lane budget そのものは real socket + real clock の
    /// unit test（`a_hung_daemon_bounds_one_keystroke_and_resolves_it_by_ledger_query` ほか）が
    /// 押さえる。ここが固定するのは product 上の事実 —— quit が**有限時間で終わる**こと —— で、
    /// 修正前はここが無期限だった。frame 予算そのものの縮小は #551 の担当である。
    const QUIT_BOUND: Duration = Duration::from_secs(45);

    let _serial = serial();
    let home = short_home();
    let workspace_root = tempfile::tempdir().unwrap();
    let workspace = workspace_root.path().join("hung-daemon-workspace");
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

    write_prompt_settings(home.path());

    let fixture = tempfile::tempdir().unwrap();
    let shell = fixture.path().join("fixture-shell");
    let spawn_count = fixture.path().join("shell-spawn-count");
    write_terminal_fixture(&shell, &spawn_count);
    let registered = home
        .command_at(
            Channel::Local,
            &workspace,
            &["open".as_ref(), workspace.as_os_str()],
        )
        .env("SHELL", &shell)
        .output()
        .expect("workspace registers with fixture login shell");
    assert!(registered.status.success());

    let (mut master, slave) = open_pty().unwrap();
    let reader_master = master.try_clone().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let reader_capture = Arc::clone(&captured);
    let reader = thread::spawn(move || read_pty_shared(reader_master, &reader_capture));

    let baseline = capture_len(&captured);
    let mut tui = spawn_hop(&home, &workspace, &slave).unwrap();
    open_registered_workspace(&mut master, &captured, baseline);
    submit_closeup_command(&mut master, &captured, baseline, "terminal open");
    wait_for_screen_since(&captured, baseline, "generic-ready-unique:");
    activate_selected_live_pane(&mut master, &captured, baseline);

    // The pane really is live and attached before the daemon is frozen, so the
    // quit below has a real subscription to release over the frozen lane rather
    // than nothing to do.
    send(&mut master, b"before-freeze\r");
    wait_for_screen_since(&captured, baseline, "generic-input:before-freeze");
    // Leave the pane for Switch while the daemon still answers; bare Ctrl-Q
    // belongs to the PTY while the live terminal owns input.
    send(&mut master, b"\x0f\x0f");
    wait_for_screen_since(&captured, baseline, "[switch]");

    // SIGSTOP is the exact hung-daemon shape: the process answers nothing while
    // its listening socket and every established connection stay open, so only a
    // client-side deadline can end a wait on it.
    let daemon = daemon_pid(home.path());
    // SAFETY: `daemon` is the pid this test's own `DaemonHome` recorded, and the
    // call only suspends that process.
    assert_eq!(
        unsafe { libc::kill(libc::pid_t::try_from(daemon).unwrap(), libc::SIGSTOP) },
        0,
        "the fixture daemon could not be suspended"
    );

    // Quitting releases the attached pane's subscription over a lane that will
    // never answer. Reaching the confirmation frame proves the render loop kept
    // drawing, and the exit proves the release did not wait forever.
    let frozen_at = Instant::now();
    send(&mut master, b"\x11");
    wait_for_screen_since(&captured, baseline, "Detach from this workspace?");
    send(&mut master, b"\r");
    let status =
        wait_with_timeout(&mut tui, QUIT_BOUND).expect("the TUI quits within a wall-clock bound");
    let elapsed = frozen_at.elapsed();

    // SAFETY: same pid, resumed so the fixture teardown can stop it normally.
    unsafe { libc::kill(libc::pid_t::try_from(daemon).unwrap(), libc::SIGCONT) };

    assert!(
        status.success(),
        "the TUI quit cleanly against a hung daemon"
    );
    assert!(
        elapsed < QUIT_BOUND,
        "redraw and quit stayed bounded against a hung daemon: {elapsed:?}"
    );

    drop(slave);
    drop(master);
    reader.join().unwrap();
    stop_daemon(&home);
}

#[test]
fn real_pty_background_terminal_exit_closes_its_tab_through_scope_inventory() {
    let _serial = serial();
    let home = short_home();
    let workspace_root = tempfile::tempdir().unwrap();
    let workspace = workspace_root.path().join("background-exit-workspace");
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

    write_prompt_settings(home.path());

    let fixture = tempfile::tempdir().unwrap();
    let shell = fixture.path().join("fixture-shell");
    let spawn_count = fixture.path().join("shell-spawn-count");
    write_terminal_fixture(&shell, &spawn_count);
    let registered = home
        .command_at(
            Channel::Local,
            &workspace,
            &["open".as_ref(), workspace.as_os_str()],
        )
        .env("SHELL", &shell)
        .output()
        .expect("workspace registers with fixture login shell");
    assert!(registered.status.success());

    let (mut master, slave) = open_pty().unwrap();
    let reader_master = master.try_clone().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let reader_capture = Arc::clone(&captured);
    let reader = thread::spawn(move || read_pty_shared(reader_master, &reader_capture));

    // The first shipping TUI launches one terminal through the Closeup command,
    // then quits. Its exact ref carries the launch scope the second terminal is
    // started in.
    let first_baseline = capture_len(&captured);
    let mut first = spawn_hop(&home, &workspace, &slave).unwrap();
    open_registered_workspace(&mut master, &captured, first_baseline);
    submit_closeup_command(&mut master, &captured, first_baseline, "terminal open");
    wait_for_screen_since(&captured, first_baseline, "generic-ready-unique:");
    let (first_terminal, _) = generic_terminal_process(home.path());
    assert!(quit_workspace(&mut master, &mut first, &captured, first_baseline).success());

    // A second terminal in the same scope, launched by another real IPC client so
    // the next TUI open restores two tabs: one foreground, one detached.
    launch_generic_terminal(home.path(), &first_terminal);
    wait_for_file_lines(&spawn_count, 2);
    let processes = generic_terminal_processes(home.path(), 2);

    let baseline = capture_len(&captured);
    let mut tui = spawn_hop(&home, &workspace, &slave).unwrap();
    open_registered_workspace(&mut master, &captured, baseline);
    wait_for_tab_count(&captured, baseline, 2);
    // The right pane replays the selected terminal, so the marker on screen names
    // the foreground process; the other one is the detached background tab.
    let foreground_pid = displayed_terminal_pid(&captured, baseline);
    let background_pid = processes
        .iter()
        .map(|(_, pid)| *pid)
        .find(|pid| *pid != foreground_pid)
        .expect("the second terminal is the background tab");

    // Kill the background terminal's process. Nothing in this TUI is attached to
    // it, so the bounded per-scope inventory observation is the only thing that
    // can make the exit visible.
    assert!(process_is_alive(background_pid));
    kill_process(background_pid);
    wait_for_tab_count(&captured, baseline, 1);

    // The foreground pane kept its own stream across that observation, and
    // observing a background tab never attached or respawned anything.
    activate_selected_live_pane(&mut master, &captured, baseline);
    send(&mut master, b"foreground-after-background-exit\r");
    wait_for_screen_since(
        &captured,
        baseline,
        "generic-input:foreground-after-background-exit",
    );
    assert_eq!(fs::read_to_string(&spawn_count).unwrap().lines().count(), 2);
    assert_eq!(displayed_terminal_pid(&captured, baseline), foreground_pid);

    assert!(quit_workspace(&mut master, &mut tui, &captured, baseline).success());
    drop(slave);
    drop(master);
    reader.join().unwrap();
}

#[test]
#[allow(clippy::too_many_lines)] // One chronological multi-open PTY lifecycle is easier to audit intact.
fn real_pty_mixed_agents_restore_intent_dismissal_and_second_reopen_without_respawn() {
    let _serial = serial();
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

    write_prompt_settings(home.path());

    let fixture_root = tempfile::tempdir().unwrap();
    let fixtures = AgentFixtures::new(fixture_root.path());
    fixtures.write();
    let codex_count = fixtures.codex_count.clone();
    let claude_count = fixtures.claude_count.clone();
    let claude_argv = fixtures.claude_argv.clone();
    let fixture_path = fixtures.path_env();

    let registered = home
        .command_at(
            Channel::Local,
            &workspace,
            &["open".as_ref(), workspace.as_os_str()],
        )
        .env("PATH", &fixture_path)
        .env(SANDBOX_PASSTHROUGH, "1")
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
    let mut first = spawn_hop_with_path(&home, &workspace, &fixture_path, &slave).unwrap();
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
    // live 配線: 両 scope の Claude は `usagi claude-sandbox … -- claude` 経由で起動し、`--settings` の
    // フック JSON を受け取る。`guard-workspace` は session 起動だけに配線される（root は OS sandbox の
    // writable root に委ねる）。
    let launched_argv = fs::read_to_string(&claude_argv).unwrap();
    let launched: Vec<&str> = launched_argv.lines().collect();
    assert_eq!(launched.len(), 2, "{launched_argv}");
    assert!(
        launched
            .iter()
            .all(|argv| argv.contains("--settings") && argv.contains("agent-phase running")),
        "{launched_argv}"
    );
    assert_eq!(
        launched
            .iter()
            .filter(|argv| argv.contains("guard-workspace"))
            .count(),
        1,
        "{launched_argv}"
    );
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
    let mut reopened = spawn_hop_with_path(&home, &workspace, &fixture_path, &slave).unwrap();
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
        spawn_hop_with_path(&home, &workspace, &fixture_path, &slave).unwrap();
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
        // Reopen clears only the dismissal. The close-time empty/generic
        // selection remains durable instead of manufacturing a background
        // Agent selection that could steal focus on another target.
        (vec![session_claude], None)
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
    let mut after_kill = spawn_hop_with_path(&home, &workspace, &fixture_path, &slave).unwrap();
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
    let mut second_reopen = spawn_hop_with_path(&home, &workspace, &fixture_path, &slave).unwrap();
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
        (vec![session_claude], None)
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

/// One live Agent process identity for `terminal`, or a panic naming the snapshot.
fn agent_process_for(processes: &[(TerminalRef, u64)], terminal: &TerminalRef) -> u64 {
    processes
        .iter()
        .find(|(candidate, _)| candidate == terminal)
        .map(|(_, pid)| *pid)
        .expect("Agent TerminalRef has a live child PID")
}

/// #544: cold restart 後の interrupted tab を、実 PTY 上の shipping TUI と実キー入力だけで
/// resume する product E2E。
///
/// `tests/agent_ipc_e2e.rs` の cold-restart flow は shipping TUI の reducer を直接呼ぶ。ここは
/// TUI binary を実 PTY で起動し、`Ctrl-O r` / `Ctrl-O x` / `reopen` の実キー入力だけで操作して、
/// 次を process 境界で押さえる。
///
/// * root と managed session の history が distinct な tab として描画され、label は closed
///   vocabulary（`Claude (interrupted)` / `Codex (interrupted)` / `Agent (interrupted)`）だけである。
/// * fresh start・TUI open・inventory・dismissal・reopen・reconnect は provider を 1 度も起動しない。
///   provider を起動するのは `Ctrl-O r` の実キー入力だけである。
/// * `Ctrl-O r` の 2 連打（double click）が daemon operation 1 件・child spawn 1 件・live tab 1 枚へ
///   収束し、resume argv が exact な provider session ID を運ぶ。選ばなかった lineage は変わらない。
/// * provider が使えない間の resume は tab を interrupted のまま残し、retry が成功する。
/// * provider ID・argv・cwd・transcript は描画 frame と log（同じ PTY へ落ちる stderr）に出ない。
#[test]
#[allow(clippy::too_many_lines)] // 1 本の cold-restart product flow を時系列のまま検証する。
fn real_pty_cold_restart_resumes_only_the_selected_interrupted_tab_from_real_keys() {
    let _serial = serial();
    let home = short_home();
    let workspace_root = tempfile::tempdir().unwrap();
    let workspace = workspace_root.path().join("agent-resume-workspace");
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

    write_prompt_settings(home.path());

    let fixture_root = tempfile::tempdir().unwrap();
    let fixtures = AgentFixtures::new(fixture_root.path());
    fixtures.write();
    let fixture_path = fixtures.path_env();

    let registered = home
        .command_at(
            Channel::Local,
            &workspace,
            &["open".as_ref(), workspace.as_os_str()],
        )
        .env("PATH", &fixture_path)
        .env(SANDBOX_PASSTHROUGH, "1")
        .output()
        .expect("workspace registers");
    assert!(registered.status.success());
    let (workspace_id, session_id) = create_session(home.path(), "resume-scope");

    // Three conversation lineages: two histories in the workspace-root scope (one
    // per provider) and one in a managed session. Mixed provider and several
    // histories inside one scope must stay separate tabs.
    let codex_terminal = launch_agent(home.path(), workspace_id, None, "codex");
    let root_claude_terminal = launch_agent(home.path(), workspace_id, None, "claude");
    let session_claude_terminal =
        launch_agent(home.path(), workspace_id, Some(session_id), "claude");
    wait_for_file_lines(&fixtures.codex_count, 1);
    wait_for_file_lines(&fixtures.claude_count, 2);

    // Claude's provider-native ID is daemon-issued at launch (`--session-id`) and
    // an exact resume must reuse that same ID (`--resume`). `guard-workspace` is
    // wired only for a managed session, so the two argv lines map to their scopes.
    let launch_argv = fixtures.claude_launch_argv();
    assert_eq!(launch_argv.len(), 2, "{launch_argv:?}");
    let root_claude_id = claude_session_id(
        launch_argv
            .iter()
            .find(|argv| !argv.contains("guard-workspace"))
            .expect("the root Claude launch is recorded"),
        "--session-id",
    );
    let session_claude_id = claude_session_id(
        launch_argv
            .iter()
            .find(|argv| argv.contains("guard-workspace"))
            .expect("the managed-session Claude launch is recorded"),
        "--session-id",
    );
    assert_ne!(root_claude_id, session_claude_id);
    let secrets = [
        CODEX_LINEAGE,
        CODEX_TRANSCRIPT,
        CODEX_CAPTURED_CWD,
        root_claude_id.as_str(),
        session_claude_id.as_str(),
        "--session-id",
        "--resume",
        "hook_event_name",
        "--dangerously-bypass-hook-trust",
        "guard-workspace",
        "claude-sandbox",
    ];

    let (mut master, slave) = open_pty().unwrap();
    let reader_master = master.try_clone().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let reader_capture = Arc::clone(&captured);
    let reader = thread::spawn(move || read_pty_shared(reader_master, &reader_capture));

    // ── 1. The shipping TUI observes all three runtimes and persists #506 tab
    // intent, so the cold restart has durable display intent to restore from.
    let seed_baseline = capture_len(&captured);
    let mut seed = spawn_hop_with_path(&home, &workspace, &fixture_path, &slave).unwrap();
    open_registered_workspace(&mut master, &captured, seed_baseline);
    let intent = wait_for_agent_tabs(home.path(), 3);
    assert_eq!(intent.workspace_id, workspace_id);
    assert!(intent.dismissed.is_empty());
    let codex = continuation_for(&intent, &codex_terminal);
    let root_claude = continuation_for(&intent, &root_claude_terminal);
    let session_claude = continuation_for(&intent, &session_claude_terminal);
    assert_eq!(
        std::collections::BTreeSet::from([codex, root_claude, session_claude]).len(),
        3,
        "each conversation keeps its own lineage"
    );
    let seeded = agent_processes(home.path(), 3);
    let doomed = [
        agent_process_for(&seeded, &codex_terminal),
        agent_process_for(&seeded, &root_claude_terminal),
        agent_process_for(&seeded, &session_claude_terminal),
    ];
    assert!(
        quit_from_switch(&mut master, &mut seed, &captured, seed_baseline).success(),
        "seeding TUI quits normally"
    );

    // ── 2. A cold failure: SIGKILL, not a `daemon stop` that retires live
    // resources. Every old PTY is genuinely gone before anything restarts.
    let (killed_daemon, killed_generation) = sigkill_daemon(home.path());
    wait_for_dead_processes(&doomed);

    // ── 3. A fresh daemon (ordinary client bootstrap) plus a fresh TUI on the real
    // PTY. Both histories of the root scope are distinct interrupted tabs, and no
    // provider ran: not for the restart, not for the open, not for the inventory.
    let cold_baseline = capture_len(&captured);
    let mut cold = spawn_hop_with_path(&home, &workspace, &fixture_path, &slave).unwrap();
    open_registered_workspace(&mut master, &captured, cold_baseline);
    wait_for_screen_since(&captured, cold_baseline, "Codex (interrupted)");
    wait_for_screen_since(&captured, cold_baseline, "Claude (interrupted)");
    let fresh_daemon = daemon_pid(home.path());
    let fresh_generation = daemon_generation(home.path());
    assert_ne!(
        fresh_daemon, killed_daemon,
        "the daemon must be a new process"
    );
    assert_ne!(fresh_generation, killed_generation);
    assert!(
        agent_processes(home.path(), 0).is_empty(),
        "no old PTY may be restored as a live Agent"
    );
    assert_eq!(fixtures.codex_spawns(), 1);
    assert_eq!(fixtures.claude_spawns(), 2);
    let cold_screen = screen_since(&captured, cold_baseline).unwrap_or_default();
    // Every rendered `(interrupted)` belongs to the closed provider vocabulary.
    let safe_labels = [
        "Claude (interrupted)",
        "Codex (interrupted)",
        "Agent (interrupted)",
    ]
    .iter()
    .map(|label| cold_screen.matches(label).count())
    .sum::<usize>();
    assert_eq!(
        cold_screen.matches("(interrupted)").count(),
        safe_labels,
        "{cold_screen}"
    );
    assert_no_sensitive_output(&captured, cold_baseline, &secrets);

    // ── 4. Enter Closeup on the root scope, select the Codex history with real
    // keys, and press `Ctrl-O r` twice. The double activation must converge onto
    // one operation, one child, and one live tab for that lineage alone.
    send(&mut master, b"\r");
    wait_for_screen_since(&captured, cold_baseline, "[closeup]");
    select_tab_by_label(&mut master, &captured, cold_baseline, "Codex (interrupted)");
    wait_for_screen_since(
        &captured,
        cold_baseline,
        "interrupted — Ctrl-O r resumes it",
    );
    send(&mut master, b"\x0fr");
    send(&mut master, b"\x0fr");
    assert_spawns_settle(&fixtures.codex_count, 2);
    let resumed_codex = agent_processes(home.path(), 1);
    let (resumed_codex_terminal, resumed_codex_pid) = resumed_codex[0].clone();
    assert!(
        !resumed_codex_terminal.fences(&codex_terminal),
        "an explicit resume must create a new terminal incarnation"
    );
    assert_eq!(
        resumed_codex_terminal.session_id, None,
        "the root lineage resumes in the root scope"
    );
    assert!(!doomed.contains(&resumed_codex_pid));
    wait_for_screen_since(
        &captured,
        cold_baseline,
        &format!("codex-resumed-unique:{resumed_codex_pid}"),
    );
    // The resume argv carries the exact provider session ID the production
    // structured capture retained — in the fixture's file, never on the screen.
    let codex_argv = fixtures.codex_launch_argv();
    assert_eq!(codex_argv.len(), 2, "{codex_argv:?}");
    assert!(!codex_argv[0].contains(CODEX_LINEAGE), "{codex_argv:?}");
    assert!(
        codex_argv[1].contains(&format!("resume {CODEX_LINEAGE}")),
        "{codex_argv:?}"
    );
    // The retained conversation accepts live input on its replacement PTY.
    send(&mut master, b"codex-after-resume\r");
    wait_for_screen_since(&captured, cold_baseline, "codex-input:codex-after-resume");
    // The lineages that were not selected are untouched.
    assert_eq!(fixtures.claude_spawns(), 2);
    let after_codex = screen_since(&captured, cold_baseline).unwrap_or_default();
    assert!(
        !after_codex.contains("Codex (interrupted)"),
        "{after_codex}"
    );
    assert!(
        after_codex.contains("Claude (interrupted)"),
        "{after_codex}"
    );

    // ── 5. A resume that the daemon refuses (the provider CLI is unavailable)
    // leaves the tab interrupted with safe feedback and spawns nothing; the retry
    // then succeeds against the same lineage.
    let hidden = fixtures.bin.join("claude.unavailable");
    fs::rename(fixtures.bin.join("claude"), &hidden).unwrap();
    select_tab_by_label(
        &mut master,
        &captured,
        cold_baseline,
        "Claude (interrupted)",
    );
    send(&mut master, b"\x0fr");
    wait_for_screen_since(
        &captured,
        cold_baseline,
        "feedback: provider resume failed; refresh Agent inventory",
    );
    assert_eq!(
        fixtures.claude_spawns(),
        2,
        "a refused resume must not spawn a provider"
    );
    let refused = screen_since(&captured, cold_baseline).unwrap_or_default();
    assert_eq!(
        selected_tab_label(&refused).as_deref(),
        Some("Claude (interrupted)"),
        "{refused}"
    );

    fs::rename(&hidden, fixtures.bin.join("claude")).unwrap();
    send(&mut master, b"\x0fr");
    assert_spawns_settle(&fixtures.claude_count, 3);
    let resumed_root = agent_processes(home.path(), 2);
    let (resumed_root_claude_terminal, resumed_root_claude_pid) = resumed_root
        .iter()
        .find(|(terminal, _)| terminal != &resumed_codex_terminal)
        .cloned()
        .expect("the retried resume produced a second live Agent");
    assert!(!resumed_root_claude_terminal.fences(&root_claude_terminal));
    assert_eq!(resumed_root_claude_terminal.session_id, None);
    wait_for_screen_since(
        &captured,
        cold_baseline,
        &format!("claude-resumed-unique:{resumed_root_claude_pid}"),
    );
    let root_resume_argv = fixtures.claude_launch_argv();
    assert!(
        root_resume_argv[2].contains(&format!("--resume {root_claude_id}")),
        "{root_resume_argv:?}"
    );
    send(&mut master, b"claude-root-after-resume\r");
    wait_for_screen_since(
        &captured,
        cold_baseline,
        "claude-input:claude-root-after-resume",
    );

    // ── 6. The managed session resumes through the same UX and fencing. Its
    // history is closed with `Ctrl-O x` (a continuation-scoped dismissal) and
    // brought back with `reopen`; neither starts a provider.
    send(&mut master, b"\x0f\x0f");
    wait_for_screen_since(&captured, cold_baseline, "[switch]");
    send(&mut master, b"\x1b[B\r");
    wait_for_screen_since(&captured, cold_baseline, "[closeup]");
    select_tab_by_label(
        &mut master,
        &captured,
        cold_baseline,
        "Claude (interrupted)",
    );
    send(&mut master, b"\x0fx");
    let dismissed = wait_for_agent_intent(home.path(), |intent| {
        intent.dismissed.contains(&session_claude)
    });
    assert_eq!(dismissed.dismissed.len(), 1);
    wait_for_screen_since(&captured, cold_baseline, "Type a command:");
    assert_eq!(
        fixtures.claude_spawns(),
        3,
        "closing a history tab must not spawn a provider"
    );
    send(
        &mut master,
        format!("reopen {}\r", session_claude.as_str()).as_bytes(),
    );
    wait_for_screen_absent_since(&captured, cold_baseline, "Type a command:");
    let _ = wait_for_agent_intent(home.path(), |intent| intent.dismissed.is_empty());
    select_tab_by_label(
        &mut master,
        &captured,
        cold_baseline,
        "Claude (interrupted)",
    );
    assert_eq!(
        fixtures.claude_spawns(),
        3,
        "reopen restores the tab without resuming the conversation"
    );

    send(&mut master, b"\x0fr");
    assert_spawns_settle(&fixtures.claude_count, 4);
    let resumed_session = agent_processes(home.path(), 3);
    let (resumed_session_terminal, resumed_session_pid) = resumed_session
        .iter()
        .find(|(terminal, _)| terminal.session_id == Some(session_id))
        .cloned()
        .expect("the managed session owns a live replacement");
    assert!(!resumed_session_terminal.fences(&session_claude_terminal));
    wait_for_screen_since(
        &captured,
        cold_baseline,
        &format!("claude-resumed-unique:{resumed_session_pid}"),
    );
    let session_resume_argv = fixtures.claude_launch_argv();
    assert!(
        session_resume_argv[3].contains(&format!("--resume {session_claude_id}")),
        "{session_resume_argv:?}"
    );
    send(&mut master, b"claude-session-after-resume\r");
    wait_for_screen_since(
        &captured,
        cold_baseline,
        "claude-input:claude-session-after-resume",
    );

    // ── 7. Reconnect: closing and reopening the TUI keeps every replacement live
    // and adds no spawn. The managed session owns exactly one tab, so its retained
    // output is the deterministic reconnect fence.
    assert!(
        quit_workspace(&mut master, &mut cold, &captured, cold_baseline).success(),
        "the resumed TUI quits normally"
    );
    let reconnect_baseline = capture_len(&captured);
    let mut reconnected = spawn_hop_with_path(&home, &workspace, &fixture_path, &slave).unwrap();
    open_registered_workspace(&mut master, &captured, reconnect_baseline);
    send(&mut master, b"\x1b[B\r");
    wait_for_screen_since(&captured, reconnect_baseline, "[closeup]");
    wait_for_screen_since(
        &captured,
        reconnect_baseline,
        "claude-input:claude-session-after-resume",
    );
    let reconnected_screen = screen_since(&captured, reconnect_baseline).unwrap_or_default();
    assert!(
        !reconnected_screen.contains("(interrupted)"),
        "every lineage converged onto its live replacement: {reconnected_screen}"
    );
    assert_eq!(fixtures.codex_spawns(), 2);
    assert_eq!(fixtures.claude_spawns(), 4);
    assert_eq!(daemon_pid(home.path()), fresh_daemon, "daemon PID changed");
    assert_eq!(
        daemon_generation(home.path()),
        fresh_generation,
        "daemon generation changed"
    );
    assert_eq!(
        agent_processes(home.path(), 3),
        resumed_session,
        "reconnect must not replace any Agent process"
    );
    assert!(
        quit_workspace(&mut master, &mut reconnected, &captured, reconnect_baseline,).success()
    );

    // Nothing in the whole cold-restart flow put a provider-native ID, argv,
    // captured cwd, or transcript path on the terminal — frame or log.
    assert_no_sensitive_output(&captured, cold_baseline, &secrets);

    // The replacement daemon was bootstrapped by a client running on this PTY, so
    // retire it explicitly before the pair closes: a daemon that outlived the test
    // while holding an inherited descriptor would leave the reader without EOF.
    stop_daemon(&home);
    drop(slave);
    drop(master);
    reader.join().unwrap();
}

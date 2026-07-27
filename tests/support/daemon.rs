//! 結合テストが起動する usagi プロセスの唯一の入口。
//!
//! daemon の workspace root は**起動時 cwd** で決まる（`current_dir()` を repo root として
//! `SessionRuntime` に渡す）。したがって cwd を指定せずに `usagi` を起動したテストは、開発者の
//! チェックアウト（`CARGO_MANIFEST_DIR`＝セッション worktree）を権威として掴んだ daemon を作って
//! しまい、その worktree の削除まで巻き込む。ここを唯一の choke point にして、
//!
//! - すべての起動で fixture workspace を `.current_dir()` に強制し、
//! - cwd がチェックアウトの内側でないことを起動ごとに assert し、
//! - fixture の teardown で daemon を record の exact incarnation で reap する
//!
//! ことを「忘れられない形」にする。直接 `Command::new(env!("CARGO_BIN_EXE_usagi"))` を書くと
//! この保証が抜けるので、テストは必ずこの module 経由で起動する。
//!
//! `daemon serve` を直接起動する経路だけでなく、`daemon start` / `daemon restart` や client
//! bootstrap（`session ...` / `mcp` / TUI）による間接起動も同じ経路に載る。

#![cfg(unix)]
// 各 test crate はこの helper の一部だけを使う（cli_tui は production 経路、pty は hop 経路など）。
#![allow(dead_code)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use usagi_core::domain::daemon::DaemonRecord;
use usagi_core::infrastructure::ipc::ClientWorkspace;

/// daemon の graceful stop を待つ上限。
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// SIGTERM / SIGKILL 後に終了を待つ上限。
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(2);

/// テストが使う runtime channel。data directory の割り付けは
/// `usagi_core::infrastructure::paths` の mode 別レイアウトに従う。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Channel {
    /// `USAGI_RUNTIME_MODE` 未設定（既定）。`<home>/local` を使う。
    Local,
    /// `USAGI_RUNTIME_MODE=production`。`<home>` を直接使う。
    Production,
}

impl Channel {
    fn data_dir(self, home: &Path) -> PathBuf {
        match self {
            // テストプロセス自身は runtime mode を設定しないため、これは `<home>/local` を返す。
            Self::Local => usagi_core::infrastructure::paths::channel_data_dir(home),
            Self::Production => home.to_path_buf(),
        }
    }

    fn apply(self, command: &mut Command) {
        match self {
            Self::Local => {}
            Self::Production => {
                command.env("USAGI_RUNTIME_MODE", "production");
            }
        }
    }
}

/// 1 テスト分の隔離環境: `$USAGI_HOME` と、起動する usagi の cwd になる fixture workspace。
///
/// drop で両 channel の daemon を reap し、起動していた daemon の workspace root が fixture
/// だったこと（開発者の worktree に束縛されていないこと）を assert する。
pub struct DaemonHome {
    home: tempfile::TempDir,
    workspace: tempfile::TempDir,
}

impl DaemonHome {
    /// Unix domain socket の sockaddr 長に収まる短い `/tmp` 配下へ home と workspace を作る。
    #[must_use]
    pub fn new() -> Self {
        let home = short_dir("usagi-");
        std::fs::set_permissions(home.path(), private_dir_mode())
            .expect("private daemon data directory");
        Self {
            home,
            workspace: short_dir("usagi-workspace-"),
        }
    }

    /// `$USAGI_HOME`。
    #[must_use]
    pub fn path(&self) -> &Path {
        self.home.path()
    }

    /// 起動する usagi プロセスの既定 cwd。daemon の workspace root になる。
    #[must_use]
    pub fn workspace(&self) -> &Path {
        self.workspace.path()
    }

    /// 既定 channel（local）の data directory。
    #[must_use]
    pub fn data_dir(&self) -> PathBuf {
        Channel::Local.data_dir(self.path())
    }

    /// production channel の data directory。
    #[must_use]
    pub fn production_data_dir(&self) -> PathBuf {
        Channel::Production.data_dir(self.path())
    }

    /// fixture workspace を cwd とする `usagi` command（local channel）。
    pub fn command(&self, args: &[&OsStr]) -> Command {
        self.command_at(Channel::Local, self.workspace(), args)
    }

    /// production channel の `usagi` command。
    pub fn production_command(&self, args: &[&OsStr]) -> Command {
        self.command_at(Channel::Production, self.workspace(), args)
    }

    /// cwd を明示する `usagi` command。`cwd` は fixture（チェックアウトの外）でなければならない。
    pub fn command_at(&self, channel: Channel, cwd: &Path, args: &[&OsStr]) -> Command {
        usagi_command(self.path(), channel, cwd, args)
    }

    /// local channel で `usagi` を実行して出力を返す。
    pub fn run(&self, args: &[&OsStr]) -> Output {
        self.command(args)
            .output()
            .expect("usagi バイナリを起動できる")
    }

    /// cwd を明示して `usagi` を実行する。
    pub fn run_at(&self, cwd: &Path, args: &[&OsStr]) -> Output {
        self.command_at(Channel::Local, cwd, args)
            .output()
            .expect("usagi バイナリを起動できる")
    }

    /// production channel で `usagi` を実行する。
    pub fn run_in_production(&self, args: &[&OsStr]) -> Output {
        self.production_command(args)
            .output()
            .expect("production runtime の usagi バイナリを起動できる")
    }

    /// production channel の `daemon serve` を、この fixture が所有する子プロセスとして起動する。
    pub fn spawn_serve(&self) -> OwnedDaemon {
        self.spawn_serve_in(Channel::Production)
    }

    /// 指定 channel の `daemon serve` を、この fixture が所有する子プロセスとして起動する。
    pub fn spawn_serve_in(&self, channel: Channel) -> OwnedDaemon {
        self.spawn_role(channel, Role::Active)
    }

    /// production channel の `daemon serve --standby` を、この fixture が所有する子プロセスと
    /// して起動する。
    ///
    /// standby は `daemon.json` に載らないため `daemon stop` の対象にならない。teardown は
    /// この子プロセスを直接落とす（[`OwnedDaemon::drop`]）。
    pub fn spawn_standby(&self) -> OwnedDaemon {
        self.spawn_role(Channel::Production, Role::Standby)
    }

    fn spawn_role(&self, channel: Channel, role: Role) -> OwnedDaemon {
        let mut args = vec![OsStr::new("daemon"), OsStr::new("serve")];
        if role == Role::Standby {
            args.push(OsStr::new("--standby"));
        }
        let mut command = self.command_at(channel, self.workspace(), &args);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        OwnedDaemon {
            home: self.path().to_path_buf(),
            channel,
            role,
            child: command.spawn().expect("daemon serve を起動できる"),
        }
    }

    /// この fixture が起動した daemon の workspace root が fixture であることを確認する。
    ///
    /// daemon は起動時 cwd を `sessions.json` の `repository_root` として durable に記録するため、
    /// 「開発者の worktree に束縛されていない」ことをプロセス外から観測できる。
    pub fn assert_fixture_workspace_root(&self) {
        for channel in [Channel::Local, Channel::Production] {
            let path = channel.data_dir(self.path()).join("daemon/sessions.json");
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let state: serde_json::Value =
                serde_json::from_slice(&bytes).expect("daemon の lifecycle state は JSON である");
            let root = state["repository_root"]
                .as_str()
                .expect("lifecycle state は workspace root を記録する");
            assert_outside_checkout(Path::new(root), "daemon の workspace root");
        }
    }

    /// この home の daemon を（起動経路に依らず）停止して reap する。
    pub fn reap(&self) {
        reap(self.path());
    }
}

impl Default for DaemonHome {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for DaemonHome {
    fn drop(&mut self) {
        // 先に reap する。assert が失敗して drop が中断されても daemon を残さない。
        self.reap();
        if !std::thread::panicking() {
            self.assert_fixture_workspace_root();
        }
    }
}

/// この fixture が直接 spawn した `daemon serve` プロセス。
///
/// `Child` は起動した exact incarnation を指すので、cleanup が pid 再利用や置き換わった
/// daemon を撃つことはない。
/// 起動した `serve` の role。teardown の経路が role で異なる。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    /// data directory の authority を取る通常の daemon。
    Active,
    /// authority を取らない standby generation。
    Standby,
}

pub struct OwnedDaemon {
    home: PathBuf,
    channel: Channel,
    role: Role,
    child: Child,
}

impl OwnedDaemon {
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// `timeout` 以内にこのプロセスが終了したか。
    pub fn wait_for_exit(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.child.try_wait().is_ok_and(|status| status.is_some()) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// SIGKILL して reap する（abnormal exit の回復経路を検証するテスト用）。
    pub fn kill_and_reap(&mut self) {
        self.child.kill().expect("SIGKILL the owned daemon");
        self.child.wait().expect("reap the killed daemon");
    }
}

impl OwnedDaemon {
    /// SIGTERM を送って graceful shutdown を待つ。standby は `daemon stop` の対象では
    /// ないため、standby を落とす唯一の協調的な経路である。
    pub fn terminate_and_wait(&mut self) -> bool {
        signal_child(self.child.id(), libc::SIGTERM);
        self.wait_for_exit(SIGNAL_TIMEOUT)
    }
}

/// 起動した exact child へ signal を送る（pid 再利用は `Child` が持つ handle が防ぐ）。
fn signal_child(pid: u32, signal: libc::c_int) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: `pid` はこの fixture が spawn した未 reap の子プロセスである。
        unsafe { libc::kill(pid, signal) };
    }
}

impl Drop for OwnedDaemon {
    fn drop(&mut self) {
        if self.role == Role::Standby {
            // standby は record を持たないので stop client が見つけられない。
            self.terminate_and_wait();
        } else {
            let _ = stop_command(&self.home, self.channel)
                .spawn()
                .map(|mut child| wait_with_timeout(&mut child, STOP_TIMEOUT));
        }
        if !self.wait_for_exit(SIGNAL_TIMEOUT) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// テスト client が handshake で申告する workspace（#548）。
///
/// daemon は起動時に確定した workspace root しか serve できず、handshake はそこから外れた
/// client を typed error で拒否する。fixture は `sessions.json` に記録された root をそのまま
/// 申告することで、「その workspace で動く実 client」と同じ経路で admit される。
///
/// socket は lifecycle state の書き込みより先に connectable になり得るため、記録が現れるまで
/// 有界に待つ。空 root を申告して拒否されると、テストの失敗理由が fence の誤りに見えてしまう。
pub fn client_workspace(data_dir: &Path) -> ClientWorkspace {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(root) = recorded_workspace_root(data_dir) {
            return ClientWorkspace::Bound { root };
        }
        assert!(
            Instant::now() < deadline,
            "daemon did not record its workspace root in {}",
            data_dir.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn recorded_workspace_root(data_dir: &Path) -> Option<String> {
    let bytes = std::fs::read(data_dir.join("daemon/sessions.json")).ok()?;
    let state: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let root = usagi_core::infrastructure::paths::wire_workspace_root(PathBuf::from(
        state["repository_root"].as_str()?,
    ));
    (!root.is_empty()).then_some(root)
}

/// テストが `usagi` を起動する唯一の command builder。
///
/// 自前の一時ディレクトリを管理するテスト（`tests/support/mcp.rs` や agent E2E）は
/// [`DaemonHome`] を使わずにこれを直接呼ぶ。どちらの経路でも cwd の検証は同じ 1 か所で効く。
pub fn usagi_command(home: &Path, channel: Channel, cwd: &Path, args: &[&OsStr]) -> Command {
    assert_outside_checkout(cwd, "テストが起動する usagi の cwd");
    let mut command = Command::new(env!("CARGO_BIN_EXE_usagi"));
    command
        .args(args)
        .current_dir(cwd)
        .env("USAGI_HOME", home)
        // hook が export する git 環境は fixture repository へ持ち込まない。
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE");
    channel.apply(&mut command);
    command
}

/// 両 channel の daemon を停止し、残っていれば record の exact incarnation を段階的に落とす。
///
/// `daemon start` / `daemon restart` / client bootstrap が間接起動した daemon は誰の `Child`
/// でもないため、`daemon.json` の pid + process-start identity を唯一の権威として reap する。
pub fn reap(home: &Path) {
    for channel in [Channel::Local, Channel::Production] {
        reap_channel(home, channel);
    }
}

fn reap_channel(home: &Path, channel: Channel) {
    let data_dir = channel.data_dir(home);
    // stop は record を消してから終了するので、識別子は先に読む。
    let record = read_record(&data_dir);
    // fake daemon を自プロセス上で立てるテストは、自分の pid と identity を record に書く。
    // その record を stop client や signal の対象にすると**テストプロセス自身**を殺すため、
    // 自分を名指す record の channel は reap しない（reap すべき別プロセスは存在しない）。
    if record
        .as_ref()
        .is_some_and(|record| record.pid == std::process::id())
    {
        return;
    }
    if let Ok(mut child) = stop_command(home, channel).spawn() {
        wait_with_timeout(&mut child, STOP_TIMEOUT);
    }
    let Some(record) = record else {
        return;
    };
    if !exact_process_alive(&record) {
        return;
    }
    signal_exact(&record, libc::SIGTERM);
    if wait_for_exit(&record, SIGNAL_TIMEOUT) {
        return;
    }
    signal_exact(&record, libc::SIGKILL);
    wait_for_exit(&record, SIGNAL_TIMEOUT);
}

fn stop_command(home: &Path, channel: Channel) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_usagi"));
    command
        // Teardown gives up whatever the fixture daemon still owns: a planned
        // stop refuses while a runtime is live (#507), which would leak the
        // process into the next test.
        .args(["daemon", "stop", "--force"])
        .env("USAGI_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    channel.apply(&mut command);
    command
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait().is_ok_and(|status| status.is_some()) {
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn read_record(data_dir: &Path) -> Option<DaemonRecord> {
    let bytes = std::fs::read(data_dir.join("daemon/daemon.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// record の pid が今もその record の incarnation か。pid 再利用を identity で弾く。
fn exact_process_alive(record: &DaemonRecord) -> bool {
    observed_process_start_identity(record.pid)
        .is_some_and(|observed| Some(observed) == record.process_start_identity)
}

fn signal_exact(record: &DaemonRecord, signal: libc::c_int) {
    if !exact_process_alive(record) {
        return;
    }
    if let Ok(pid) = libc::pid_t::try_from(record.pid) {
        // SAFETY: identity を直前に再確認した、このテストの home が記録した daemon のみを撃つ。
        unsafe { libc::kill(pid, signal) };
    }
}

fn wait_for_exit(record: &DaemonRecord, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while exact_process_alive(record) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

/// `pid` の OS process-start identity。fixture が daemon record を組み立てるときに使う。
///
/// # Panics
///
/// 観測できない場合（プロセスが既に消えている場合を含む）panic する。
#[must_use]
pub fn process_start_identity(pid: u32) -> String {
    observed_process_start_identity(pid).expect("process-start identity を観測できる")
}

#[cfg(target_os = "linux")]
fn observed_process_start_identity(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    let start_ticks = stat[close + 1..].split_whitespace().nth(19)?;
    start_ticks.parse::<u64>().ok()?;
    Some(format!("linux:{start_ticks}"))
}

#[cfg(target_os = "macos")]
fn observed_process_start_identity(pid: u32) -> Option<String> {
    let pid = libc::pid_t::try_from(pid).ok()?;
    // SAFETY: `info` は初期化済みで、渡す長さは proc_bsdinfo の実サイズと一致する。
    let mut info = unsafe { std::mem::zeroed::<libc::proc_bsdinfo>() };
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: 上と同じ初期化済みバッファを渡す。
    let read =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    (read == size).then(|| format!("macos:{}:{}", info.pbi_start_tvsec, info.pbi_start_tvusec))
}

/// Unix domain socket の sockaddr 長に収まる短い一時ディレクトリ。
#[must_use]
pub fn short_dir(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in("/tmp")
        .expect("short paths keep Unix sockets below platform limits")
}

fn private_dir_mode() -> std::fs::Permissions {
    use std::os::unix::fs::PermissionsExt;

    std::fs::Permissions::from_mode(0o700)
}

/// `path` が開発者のチェックアウト（`CARGO_MANIFEST_DIR`）の内側でないことを確認する。
///
/// daemon はここを権威として git worktree と branch を所有するため、内側を指した瞬間に
/// 「テストが開発者の worktree を掴む」回帰になる。
fn assert_outside_checkout(path: &Path, what: &str) {
    let checkout = canonical(Path::new(env!("CARGO_MANIFEST_DIR")));
    let observed = canonical(path);
    assert!(
        !observed.starts_with(&checkout),
        "{what} は fixture でなければならない（開発者のチェックアウト {} の内側だった: {}）",
        checkout.display(),
        observed.display()
    );
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

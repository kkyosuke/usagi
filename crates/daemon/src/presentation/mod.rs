//! daemon サーバの presentation 層。daemon 面の entry point と、IPC リクエストの
//! dispatch・応答整形を持ち、ロジックは usagi-core の usecase（監視・store 系）と
//! 本クレートの daemon 専用 usecase（`crate::usecase`）へ委譲する。
//! 実 socket・PTY は合成ルートが束ね、この層は注入された入出力に対して純粋に振る舞う。
//! v2 では必要になった時点で端点を追加する。

use std::io::Write;

use usagi_core::domain::AppInfo;
use usagi_core::infrastructure::daemon::{
    DaemonLauncher, DaemonReady, DaemonRecordStore, InstanceLock, LivenessProbe,
    ProcessIdentitySource, RecordFile, ShutdownSignal, Sleeper, Terminator,
};

use crate::usecase;

pub mod ipc;

/// 合成ルートで検証済みの daemon 制御要求。
///
/// argv の文字列解釈と usage error の整形は合成ルートが担い、この層には実行可能な
/// verb だけを閉じた型として渡す。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DaemonCommand {
    /// 前景で daemon を常駐させる。
    Serve,
    /// daemon を背景起動する。
    Start,
    /// daemon の稼働状態を表示する。
    Status,
    /// 稼働中の daemon を停止する。
    Stop,
    /// daemon を停止してから背景起動する。
    Restart,
}

/// daemon 面が実 IO を行うために注入される依存一式。合成ルートが本物（ファイル・
/// process-start identity 観測・fenced SIGTERM・signal 待受・detached spawn・sleep・単一インスタンスロック・
/// 自プロセス pid）を束ねて構築し、テストは fake を差し込む。[`run`] にまとめて渡すことで、
/// verb ごとに必要な seam が増えても entry point の引数を平らに保つ。
pub struct DaemonEnv<'a, F, P, T, R, S, L, K, M> {
    /// `daemon.json` の read/write/incarnation-conditional clear。
    pub store: &'a DaemonRecordStore<F>,
    /// daemon owner の exact process identity 観測。
    pub probe: &'a P,
    /// 稼働中 daemon への終了要求（signal）。
    pub terminator: &'a T,
    /// `serve` が exact owner record 登録後に IPC endpoint を公開する ready hook。
    pub ready: &'a R,
    /// `serve` が shutdown まで待つための待受。
    pub shutdown: &'a S,
    /// `start` が detached `serve` を spawn するための起動器。
    pub launcher: &'a L,
    /// `start` の登録確認と `stop` の owner cleanup 確認で待つ sleeper。
    pub sleeper: &'a K,
    /// `serve` の単一インスタンスロック（多重起動を防ぐ権威）。
    pub lock: &'a M,
    /// `serve` が register する自プロセスの pid。
    pub pid: u32,
}

/// daemon 面の entry point。合成ルートが `usagi daemon` の argv を検証して構築した
/// [`DaemonCommand`] を受け取り、結果を注入された `out` へ書き出す。この層は振り分けと
/// 書き出しの配線に徹し、独自のビジネスロジックは持たない。
///
/// 実 IO を伴う verb は、注入された [`DaemonEnv`] を使う usecase へ振り分ける:
/// `serve` は前景の常駐 [`usecase::serve::serve`]、`start` は背景起動の
/// [`usecase::start::start`]、`status` は [`usecase::status::report`]、`stop` は
/// [`usecase::stop::stop`]、`restart` は [`usecase::restart::restart`]。
///
/// # Errors
///
/// 振り分け先 usecase のレコード読取・signal・待受・spawn・掃除に失敗した場合、または `out`
/// への書き込みに失敗した場合、そのエラーを返す。
pub fn run<
    F: RecordFile,
    P: LivenessProbe + ProcessIdentitySource,
    T: Terminator,
    S: ShutdownSignal,
    R: DaemonReady + usecase::stop::StaleDaemonCleanup,
    L: DaemonLauncher,
    K: Sleeper,
    M: InstanceLock,
>(
    out: &mut dyn Write,
    command: DaemonCommand,
    info: &AppInfo,
    env: &DaemonEnv<F, P, T, R, S, L, K, M>,
) -> std::io::Result<()> {
    match command {
        DaemonCommand::Serve => usecase::serve::serve(
            out,
            env.store,
            env.ready,
            env.shutdown,
            env.lock,
            env.probe,
            env.pid,
            info,
        ),
        DaemonCommand::Start => {
            let line =
                usecase::start::start(env.store, env.probe, env.launcher, env.sleeper, info)?;
            writeln!(out, "{line}")
        }
        DaemonCommand::Status => {
            let line = usecase::status::report(env.store, env.probe, info)?;
            writeln!(out, "{line}")
        }
        DaemonCommand::Stop => {
            let line = usecase::stop::stop(
                env.store,
                env.probe,
                env.terminator,
                env.sleeper,
                env.ready,
                info,
            )?;
            writeln!(out, "{line}")
        }
        DaemonCommand::Restart => {
            let line = usecase::restart::restart(
                env.store,
                env.probe,
                env.terminator,
                env.launcher,
                env.sleeper,
                env.ready,
                info,
            )?;
            writeln!(out, "{line}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DaemonCommand, DaemonEnv, run};
    use crate::test_support::{
        FakeLock, FixedProbe, ImmediateShutdown, InMemoryRecordFile, NoopReady, NoopSleeper,
        RecordingTerminator, TestLauncher,
    };
    use usagi_core::domain::AppInfo;
    use usagi_core::domain::daemon::DaemonRecord;
    use usagi_core::infrastructure::daemon::DaemonRecordStore;

    fn info() -> AppInfo {
        AppInfo {
            name: "usagi",
            version: "0.1.0",
        }
    }

    /// Run `command` against a live-probe env (idle launcher — `start` not
    /// exercised here) and return what was written.
    fn run_line(command: DaemonCommand, store: &DaemonRecordStore<InMemoryRecordFile>) -> String {
        let (probe, terminator, shutdown, sleeper) = (
            FixedProbe(true),
            RecordingTerminator::default(),
            ImmediateShutdown,
            NoopSleeper,
        );
        let launcher = TestLauncher::idle(store);
        let ready = NoopReady;
        let env = DaemonEnv {
            store,
            probe: &probe,
            terminator: &terminator,
            ready: &ready,
            shutdown: &shutdown,
            launcher: &launcher,
            sleeper: &sleeper,
            lock: &FakeLock::Acquired,
            pid: 4321,
        };
        let mut buf = Vec::new();
        run(&mut buf, command, &info(), &env).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn run_routes_serve_to_the_foreground_server() {
        // With no record and an immediate shutdown, serve registers, then clears.
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        assert_eq!(
            run_line(DaemonCommand::Serve, &store),
            "usagi v0.1.0: daemon serving (pid 4321)\nusagi v0.1.0: daemon stopped (pid 4321)\n"
        );
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn run_routes_start_and_restart_to_the_launcher() {
        // Both start and restart launch a daemon; the launcher registers pid 5555.
        for (command, expected) in [
            (
                DaemonCommand::Start,
                "usagi v0.1.0: daemon started (pid 5555)\n",
            ),
            (
                DaemonCommand::Restart,
                "usagi v0.1.0: daemon restarted (pid 5555)\n",
            ),
        ] {
            let store = DaemonRecordStore::new(InMemoryRecordFile::default());
            let (probe, terminator, shutdown, sleeper) = (
                FixedProbe(true),
                RecordingTerminator::default(),
                ImmediateShutdown,
                NoopSleeper,
            );
            let launcher = TestLauncher::registering(&store, 5555);
            let ready = NoopReady;
            let env = DaemonEnv {
                store: &store,
                probe: &probe,
                terminator: &terminator,
                ready: &ready,
                shutdown: &shutdown,
                launcher: &launcher,
                sleeper: &sleeper,
                lock: &FakeLock::Acquired,
                pid: 4321,
            };
            let mut buf = Vec::new();
            run(&mut buf, command, &info(), &env).unwrap();
            assert_eq!(String::from_utf8(buf).unwrap(), expected);
        }
    }

    #[test]
    fn run_routes_stop_to_the_record_backed_stop() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        // No record yet: stop reports there is nothing to stop.
        assert_eq!(
            run_line(DaemonCommand::Stop, &store),
            "usagi v0.1.0: daemon not running\n"
        );
    }

    #[test]
    fn run_routes_status_to_the_record_backed_report() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        // No record yet: status reports the daemon is not running.
        assert_eq!(
            run_line(DaemonCommand::Status, &store),
            "usagi v0.1.0: daemon not running\n"
        );
        // With a live record, status reports it running with its pid.
        store.save(&DaemonRecord::new(4321)).unwrap();
        assert_eq!(
            run_line(DaemonCommand::Status, &store),
            "usagi v0.1.0: daemon running (pid 4321)\n"
        );
    }

    #[test]
    fn run_propagates_usecase_errors() {
        let (probe, terminator, shutdown, sleeper) = (
            FixedProbe(true),
            RecordingTerminator::default(),
            ImmediateShutdown,
            NoopSleeper,
        );
        // `serve` on the acquired path writes without reading, so a malformed
        // record does not surface there; its error paths are covered in its own
        // tests. The record-reading verbs must propagate the load error.
        for command in [
            DaemonCommand::Status,
            DaemonCommand::Stop,
            DaemonCommand::Start,
            DaemonCommand::Restart,
        ] {
            let store = DaemonRecordStore::new(InMemoryRecordFile::with("not json"));
            let launcher = TestLauncher::idle(&store);
            let ready = NoopReady;
            let env = DaemonEnv {
                store: &store,
                probe: &probe,
                terminator: &terminator,
                ready: &ready,
                shutdown: &shutdown,
                launcher: &launcher,
                sleeper: &sleeper,
                lock: &FakeLock::Acquired,
                pid: 4321,
            };
            let mut buf = Vec::new();
            assert!(run(&mut buf, command, &info(), &env).is_err());
        }
    }
}

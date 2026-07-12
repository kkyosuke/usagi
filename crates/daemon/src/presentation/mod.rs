//! daemon サーバの presentation 層。daemon 面の entry point と、IPC リクエストの
//! dispatch・応答整形を持ち、ロジックは usagi-core の usecase（監視・store 系）と
//! 本クレートの daemon 専用 usecase（`crate::usecase`）へ委譲する。
//! 実 socket・PTY は合成ルートが束ね、この層は注入された入出力に対して純粋に振る舞う。
//! v2 では必要になった時点で端点を追加する。

use std::io::Write;

use usagi_core::domain::AppInfo;
use usagi_core::infrastructure::daemon::{
    DaemonLauncher, DaemonRecordStore, InstanceLock, LivenessProbe, RecordFile, ShutdownSignal,
    Sleeper, Terminator,
};

use crate::usecase;

/// daemon 面が実 IO を行うために注入される依存一式。合成ルートが本物（ファイル・
/// signal 0・SIGTERM・signal 待受・detached spawn・sleep・単一インスタンスロック・
/// 自プロセス pid）を束ねて構築し、テストは fake を差し込む。[`run`] にまとめて渡すことで、
/// verb ごとに必要な seam が増えても entry point の引数を平らに保つ。
pub struct DaemonEnv<'a, F, P, T, S, L, K, M> {
    /// `daemon.json` の read/write/clear。
    pub store: &'a DaemonRecordStore<F>,
    /// pid の生存判定。
    pub probe: &'a P,
    /// 稼働中 daemon への終了要求（signal）。
    pub terminator: &'a T,
    /// `serve` が shutdown まで待つための待受。
    pub shutdown: &'a S,
    /// `start` が detached `serve` を spawn するための起動器。
    pub launcher: &'a L,
    /// `start` が登録確認ポーリングの間に待つための sleeper。
    pub sleeper: &'a K,
    /// `serve` の単一インスタンスロック（多重起動を防ぐ権威）。
    pub lock: &'a M,
    /// `serve` が register する自プロセスの pid。
    pub pid: u32,
}

/// daemon 面の entry point。合成ルートが `usagi daemon` で dispatch する interface で、
/// `usagi daemon` に続くサブコマンド（無しは `None`）を解決し、結果を注入された `out` へ
/// 書き出す。この層は解決と書き出しの配線に徹し、独自のビジネスロジックは持たない。
///
/// 実 IO を伴う verb は、注入された [`DaemonEnv`] を使う usecase へ振り分ける:
/// 無指定と `serve` は前景の常駐 [`usecase::serve::serve`]、`start` は背景起動の
/// [`usecase::start::start`]、`status` は [`usecase::status::report`]、`stop` は
/// [`usecase::stop::stop`]、`restart` は [`usecase::restart::restart`]。認識できない
/// サブコマンドは [`usecase::unknown_subcommand`] の案内を書き出す。
///
/// # Errors
///
/// 振り分け先 usecase のレコード読取・signal・待受・spawn・掃除に失敗した場合、または `out`
/// への書き込みに失敗した場合、そのエラーを返す。
pub fn run<
    W: Write,
    F: RecordFile,
    P: LivenessProbe,
    T: Terminator,
    S: ShutdownSignal,
    L: DaemonLauncher,
    K: Sleeper,
    M: InstanceLock,
>(
    out: &mut W,
    subcommand: Option<&str>,
    info: &AppInfo,
    env: &DaemonEnv<F, P, T, S, L, K, M>,
) -> std::io::Result<()> {
    match subcommand {
        None | Some("serve") => {
            usecase::serve::serve(out, env.store, env.shutdown, env.lock, env.pid, info)
        }
        Some("start") => {
            let line =
                usecase::start::start(env.store, env.probe, env.launcher, env.sleeper, info)?;
            writeln!(out, "{line}")
        }
        Some("status") => {
            let line = usecase::status::report(env.store, env.probe, info)?;
            writeln!(out, "{line}")
        }
        Some("stop") => {
            let line = usecase::stop::stop(env.store, env.probe, env.terminator, info)?;
            writeln!(out, "{line}")
        }
        Some("restart") => {
            let line = usecase::restart::restart(
                env.store,
                env.probe,
                env.terminator,
                env.launcher,
                env.sleeper,
                info,
            )?;
            writeln!(out, "{line}")
        }
        Some(other) => writeln!(out, "{}", usecase::unknown_subcommand(info, other)),
    }
}

#[cfg(test)]
mod tests {
    use super::{DaemonEnv, run};
    use crate::test_support::{
        FakeLock, FixedProbe, ImmediateShutdown, InMemoryRecordFile, NoopSleeper,
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

    /// Run `subcommand` against a live-probe env (idle launcher — `start` not
    /// exercised here) and return what was written.
    fn run_line(subcommand: Option<&str>, store: &DaemonRecordStore<InMemoryRecordFile>) -> String {
        let (probe, terminator, shutdown, sleeper) = (
            FixedProbe(true),
            RecordingTerminator::default(),
            ImmediateShutdown,
            NoopSleeper,
        );
        let launcher = TestLauncher::idle(store);
        let env = DaemonEnv {
            store,
            probe: &probe,
            terminator: &terminator,
            shutdown: &shutdown,
            launcher: &launcher,
            sleeper: &sleeper,
            lock: &FakeLock::Acquired,
            pid: 4321,
        };
        let mut buf = Vec::new();
        run(&mut buf, subcommand, &info(), &env).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn run_serves_on_none_and_serve() {
        // With no record and an immediate shutdown, serve registers, then clears.
        for subcommand in [None, Some("serve")] {
            let store = DaemonRecordStore::new(InMemoryRecordFile::default());
            assert_eq!(
                run_line(subcommand, &store),
                "usagi v0.1.0: daemon serving (pid 4321)\nusagi v0.1.0: daemon stopped (pid 4321)\n"
            );
            assert_eq!(store.load().unwrap(), None);
        }
    }

    #[test]
    fn run_reports_unknown_subcommand() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        assert_eq!(
            run_line(Some("bogus"), &store),
            "usagi v0.1.0: unknown daemon subcommand `bogus`\n"
        );
    }

    #[test]
    fn run_routes_start_and_restart_to_the_launcher() {
        // Both start and restart launch a daemon; the launcher registers pid 5555.
        for (subcommand, expected) in [
            ("start", "usagi v0.1.0: daemon started (pid 5555)\n"),
            ("restart", "usagi v0.1.0: daemon restarted (pid 5555)\n"),
        ] {
            let store = DaemonRecordStore::new(InMemoryRecordFile::default());
            let (probe, terminator, shutdown, sleeper) = (
                FixedProbe(true),
                RecordingTerminator::default(),
                ImmediateShutdown,
                NoopSleeper,
            );
            let launcher = TestLauncher::registering(&store, 5555);
            let env = DaemonEnv {
                store: &store,
                probe: &probe,
                terminator: &terminator,
                shutdown: &shutdown,
                launcher: &launcher,
                sleeper: &sleeper,
                lock: &FakeLock::Acquired,
                pid: 4321,
            };
            let mut buf = Vec::new();
            run(&mut buf, Some(subcommand), &info(), &env).unwrap();
            assert_eq!(String::from_utf8(buf).unwrap(), expected);
        }
    }

    #[test]
    fn run_routes_stop_to_the_record_backed_stop() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        // No record yet: stop reports there is nothing to stop.
        assert_eq!(
            run_line(Some("stop"), &store),
            "usagi v0.1.0: daemon not running\n"
        );
        // With a live record, stop terminates it and clears the record.
        store.save(&DaemonRecord::new(4321)).unwrap();
        assert_eq!(
            run_line(Some("stop"), &store),
            "usagi v0.1.0: daemon stopped (pid 4321)\n"
        );
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn run_routes_status_to_the_record_backed_report() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        // No record yet: status reports the daemon is not running.
        assert_eq!(
            run_line(Some("status"), &store),
            "usagi v0.1.0: daemon not running\n"
        );
        // With a live record, status reports it running with its pid.
        store.save(&DaemonRecord::new(4321)).unwrap();
        assert_eq!(
            run_line(Some("status"), &store),
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
        for subcommand in [Some("status"), Some("stop"), Some("start"), Some("restart")] {
            let store = DaemonRecordStore::new(InMemoryRecordFile::with("not json"));
            let launcher = TestLauncher::idle(&store);
            let env = DaemonEnv {
                store: &store,
                probe: &probe,
                terminator: &terminator,
                shutdown: &shutdown,
                launcher: &launcher,
                sleeper: &sleeper,
                lock: &FakeLock::Acquired,
                pid: 4321,
            };
            let mut buf = Vec::new();
            assert!(run(&mut buf, subcommand, &info(), &env).is_err());
        }
    }
}

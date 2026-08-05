//! daemon サーバの presentation 層。daemon 面の entry point と、IPC リクエストの
//! dispatch・応答整形を持ち、ロジックは usagi-core の usecase（監視・store 系）と
//! 本クレートの daemon 専用 usecase（`crate::usecase`）へ委譲する。
//! 実 socket・PTY は合成ルートが束ね、この層は注入された入出力に対して純粋に振る舞う。
//! v2 では必要になった時点で端点を追加する。

use std::io::Write;

use usagi_core::domain::AppInfo;
use usagi_core::infrastructure::daemon::{
    DaemonLauncher, DaemonReady, DaemonRecordStore, InstanceLock, LivenessProbe,
    ProcessIdentitySource, RecordFile, ShutdownSignal, Sleeper, Terminator, WorkspaceFence,
};

use crate::usecase;

pub mod ipc;

/// `serve` がどの role で常駐するか。
///
/// 同じ data directory に同時に存在できるのは active 1 つだが、standby は
/// **何も所有しない**ため active の隣で走れる（[`usecase::serve_standby`] が正本）。
/// role は起動時に固定され、process の途中で変わることはない（standby から active への
/// 昇格は handoff であり、別 process の仕事である）。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServeRole {
    /// data directory の authority を取り、endpoint を publish して runtime を所有する。
    Active,
    /// private endpoint だけを bind し、registry に standby として登録される。
    Standby,
}

/// 合成ルートで検証済みの daemon 制御要求。
///
/// argv の文字列解釈と usage error の整形は合成ルートが担い、この層には実行可能な
/// verb だけを閉じた型として渡す。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DaemonCommand {
    /// 前景で daemon を常駐させる。
    Serve(ServeRole),
    /// daemon を背景起動する。
    Start,
    /// daemon の稼働状態を表示する。
    Status,
    /// 稼働中の daemon を停止する。live runtime を持つ daemon は
    /// [`usecase::replacement::TransitionMode::Cold`] を明示したときだけ止まる。
    Stop(usecase::replacement::TransitionMode),
    /// daemon を入れ替える。manual restart と build/update replacement は同じ
    /// [`usecase::replacement`] の path を通る。
    Replace {
        /// この入れ替えを識別する durable operation。未知 artifact では `None`。
        operation: Option<usagi_core::infrastructure::ipc::OperationId>,
        mode: usecase::replacement::TransitionMode,
    },
}

/// daemon 面が実 IO を行うために注入される依存一式。合成ルートが本物（ファイル・
/// process-start identity 観測・fenced SIGTERM・signal 待受・detached spawn・sleep・単一インスタンスロック・
/// 自プロセス pid）を束ねて構築し、テストは fake を差し込む。[`run`] にまとめて渡すことで、
/// verb ごとに必要な seam が増えても entry point の引数を平らに保つ。
pub struct DaemonEnv<'a, F, P, T, R, S, L, K, M, W> {
    /// `daemon.json` の read/write/incarnation-conditional clear。
    pub store: &'a DaemonRecordStore<F>,
    /// daemon owner の exact process identity 観測。
    pub probe: &'a P,
    /// 稼働中 daemon への終了要求（signal）。
    pub terminator: &'a T,
    /// `serve` が exact owner record 登録後に IPC endpoint を bind する ready hook。
    pub ready: &'a R,
    /// `serve` が endpoint 応答後に durable generation registry の authority を
    /// 取得・返却する hook。locator の公開はこの authority が行う。
    pub authority: &'a dyn usecase::serve::GenerationAuthority,
    /// `serve --standby` が bind する private endpoint。locator は公開しない。
    pub standby_endpoint: &'a dyn usecase::serve_standby::StandbyEndpoint,
    /// `serve --standby` が registry へ standby として登録・返却する hook。
    pub standby_authority: &'a dyn usecase::serve_standby::StandbyAuthority,
    /// `serve` が shutdown まで待つための待受。
    pub shutdown: &'a S,
    /// `start` が detached `serve` を spawn するための起動器。
    pub launcher: &'a L,
    /// `start` の登録確認と `stop` の owner cleanup 確認で待つ sleeper。
    pub sleeper: &'a K,
    /// `serve` の単一インスタンスロック（同一 data directory の多重起動を防ぐ権威）。
    pub lock: &'a M,
    /// `serve` の workspace fence（同一 workspace の多重所有を防ぐ権威）。mode や
    /// `$USAGI_HOME` の表記差では回避できない。
    pub workspace: &'a W,
    /// `serve` が register する自プロセスの pid。
    pub pid: u32,
    /// `stop` / `replace` が壊しうる live runtime の実測。
    pub census: &'a dyn usecase::replacement::ResourceCensus,
    /// この build が live successor へ authority を渡せない理由。durable な
    /// generation registry の観測から導く。
    pub seamless: Option<usecase::replacement::SeamlessRefusal>,
    /// standby の staging と old active への rollover IPC 要求。
    pub rollover: &'a dyn usecase::replacement::RolloverRequester,
}

/// daemon 面の entry point。合成ルートが `usagi daemon` の argv を検証して構築した
/// [`DaemonCommand`] を受け取り、結果を注入された `out` へ書き出す。この層は振り分けと
/// 書き出しの配線に徹し、独自のビジネスロジックは持たない。
///
/// 実 IO を伴う verb は、注入された [`DaemonEnv`] を使う usecase へ振り分ける:
/// `serve` は role によって前景の常駐 [`usecase::serve::serve`]（active）と
/// [`usecase::serve_standby::serve_standby`]（standby）へ分かれ、`start` は背景起動の
/// [`usecase::start::start`]、`status` は [`usecase::status::report`]。
/// `stop` と `replace` は [`usecase::replacement`] を通り、live runtime を壊す遷移を
/// そこで一度だけ判定してから [`usecase::stop::stop`] /
/// [`usecase::restart::restart`] へ降りる。
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
    W: WorkspaceFence,
>(
    out: &mut dyn Write,
    command: DaemonCommand,
    info: &AppInfo,
    env: &DaemonEnv<F, P, T, R, S, L, K, M, W>,
) -> std::io::Result<()> {
    match command {
        DaemonCommand::Serve(ServeRole::Active) => usecase::serve::serve(
            out,
            env.store,
            env.ready,
            env.authority,
            env.shutdown,
            env.workspace,
            env.lock,
            env.probe,
            env.pid,
            info,
        ),
        // A standby owns nothing, so it reaches neither guard and touches
        // neither the lifecycle record nor the locator.
        DaemonCommand::Serve(ServeRole::Standby) => usecase::serve_standby::serve_standby(
            out,
            env.standby_endpoint,
            env.standby_authority,
            env.shutdown,
            env.pid,
            info,
        ),
        DaemonCommand::Start => {
            let line = usecase::start::start(
                env.store,
                env.probe,
                env.launcher,
                env.sleeper,
                env.ready,
                info,
            )?;
            writeln!(out, "{line}")
        }
        DaemonCommand::Status => {
            let line = usecase::status::report(env.store, env.probe, info)?;
            writeln!(out, "{line}")
        }
        DaemonCommand::Stop(mode) => {
            let line = usecase::replacement::stop_daemon(
                env.store,
                env.probe,
                env.terminator,
                env.sleeper,
                env.ready,
                env.census,
                mode,
                info,
            )?;
            writeln!(out, "{line}")
        }
        DaemonCommand::Replace { operation, mode } => {
            let line = usecase::replacement::replace_daemon(
                env.store,
                env.probe,
                env.terminator,
                env.launcher,
                env.sleeper,
                env.ready,
                env.census,
                env.seamless.as_ref(),
                env.rollover,
                mode,
                operation.as_ref(),
                info,
            )?;
            writeln!(out, "{line}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DaemonCommand, DaemonEnv, ServeRole, run};
    use crate::test_support::{
        CountingStandbyAuthority, FakeAuthority, FakeLock, FakeWorkspaceFence, FixedProbe,
        ImmediateShutdown, InMemoryRecordFile, NoopReady, NoopSleeper, NoopStandbyEndpoint,
        RecordingTerminator, TestLauncher,
    };
    use crate::usecase::replacement::{
        LiveResources, ResourceCensus, RolloverRequester, SeamlessRefusal, TransitionMode,
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

    /// A daemon owning `agents` Agent runtimes and nothing else.
    struct Owning(usize);
    impl ResourceCensus for Owning {
        fn live(&self) -> std::io::Result<LiveResources> {
            Ok(LiveResources {
                agents: self.0,
                terminals: 0,
            })
        }
    }
    struct NoopRollover;
    impl RolloverRequester for NoopRollover {
        fn rollover(
            &self,
            operation: &usagi_core::infrastructure::ipc::OperationId,
        ) -> std::io::Result<String> {
            Ok(format!("rolled over {}", operation.0))
        }
    }

    /// An ordinary stop: nothing live, nothing given up.
    const fn stop() -> DaemonCommand {
        DaemonCommand::Stop(TransitionMode::Planned)
    }

    /// An ordinary replacement, unkeyed.
    const fn replace() -> DaemonCommand {
        DaemonCommand::Replace {
            operation: None,
            mode: TransitionMode::Planned,
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
            authority: &FakeAuthority::default(),
            standby_endpoint: &NoopStandbyEndpoint,
            standby_authority: &CountingStandbyAuthority::default(),
            store,
            probe: &probe,
            terminator: &terminator,
            ready: &ready,
            shutdown: &shutdown,
            launcher: &launcher,
            sleeper: &sleeper,
            lock: &FakeLock::Acquired,
            workspace: &FakeWorkspaceFence::Acquired,
            pid: 4321,
            census: &Owning(0),
            seamless: Some(SeamlessRefusal::NoGenerationRegistry),
            rollover: &NoopRollover,
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
            run_line(DaemonCommand::Serve(ServeRole::Active), &store),
            "usagi v0.1.0: daemon serving (pid 4321)\nusagi v0.1.0: daemon stopped (pid 4321)\n"
        );
        assert_eq!(store.load().unwrap(), None);
    }

    /// The standby role reaches the standby state machine, and reaching it is
    /// observable in what it did *not* do: no lifecycle record was written, so
    /// the data directory's owner is untouched.
    #[test]
    fn run_routes_the_standby_role_to_the_standby_server() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let owner = DaemonRecord::identified(1111, "test:1111");
        store.save(&owner).unwrap();
        let (probe, terminator, shutdown, sleeper) = (
            FixedProbe(true),
            RecordingTerminator::default(),
            ImmediateShutdown,
            NoopSleeper,
        );
        let launcher = TestLauncher::idle(&store);
        let ready = NoopReady;
        let standby_authority = CountingStandbyAuthority::default();
        let env = DaemonEnv {
            authority: &FakeAuthority::default(),
            standby_endpoint: &NoopStandbyEndpoint,
            standby_authority: &standby_authority,
            store: &store,
            probe: &probe,
            terminator: &terminator,
            ready: &ready,
            shutdown: &shutdown,
            launcher: &launcher,
            sleeper: &sleeper,
            // A standby reaches neither guard, so a held one cannot refuse it.
            lock: &FakeLock::Held,
            workspace: &FakeWorkspaceFence::Held(1111),
            pid: 4321,
            census: &Owning(0),
            seamless: Some(SeamlessRefusal::NoLiveRegisteredActive),
            rollover: &NoopRollover,
        };
        let mut buf = Vec::new();
        run(
            &mut buf,
            DaemonCommand::Serve(ServeRole::Standby),
            &info(),
            &env,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "usagi v0.1.0: daemon standing by (pid 4321)\nusagi v0.1.0: daemon standby stopped (pid 4321)\n"
        );
        assert_eq!(standby_authority.admits(), 1);
        assert_eq!(store.load().unwrap(), Some(owner));
    }

    #[test]
    fn run_routes_start_and_replace_to_the_launcher() {
        // Both start and replace launch a daemon; the launcher registers pid 5555.
        for (command, expected) in [
            (
                DaemonCommand::Start,
                "usagi v0.1.0: daemon started (pid 5555)\n",
            ),
            (replace(), "usagi v0.1.0: daemon restarted (pid 5555)\n"),
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
                authority: &FakeAuthority::default(),
                standby_endpoint: &NoopStandbyEndpoint,
                standby_authority: &CountingStandbyAuthority::default(),
                store: &store,
                probe: &probe,
                terminator: &terminator,
                ready: &ready,
                shutdown: &shutdown,
                launcher: &launcher,
                sleeper: &sleeper,
                lock: &FakeLock::Acquired,
                workspace: &FakeWorkspaceFence::Acquired,
                pid: 4321,
                census: &Owning(0),
                seamless: Some(SeamlessRefusal::NoGenerationRegistry),
                rollover: &NoopRollover,
            };
            let mut buf = Vec::new();
            run(&mut buf, command, &info(), &env).unwrap();
            assert_eq!(String::from_utf8(buf).unwrap(), expected);
        }
    }

    #[test]
    fn run_routes_a_live_planned_replacement_to_the_rollover_port() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        store.save(&DaemonRecord::new(4321)).unwrap();
        let (probe, terminator, shutdown, sleeper) = (
            FixedProbe(true),
            RecordingTerminator::default(),
            ImmediateShutdown,
            NoopSleeper,
        );
        let launcher = TestLauncher::idle(&store);
        let ready = NoopReady;
        let env = DaemonEnv {
            authority: &FakeAuthority::default(),
            standby_endpoint: &NoopStandbyEndpoint,
            standby_authority: &CountingStandbyAuthority::default(),
            store: &store,
            probe: &probe,
            terminator: &terminator,
            ready: &ready,
            shutdown: &shutdown,
            launcher: &launcher,
            sleeper: &sleeper,
            lock: &FakeLock::Acquired,
            workspace: &FakeWorkspaceFence::Acquired,
            pid: 4321,
            census: &Owning(1),
            seamless: None,
            rollover: &NoopRollover,
        };
        let mut buf = Vec::new();
        run(
            &mut buf,
            DaemonCommand::Replace {
                operation: Some(usagi_core::infrastructure::ipc::OperationId(
                    "build-rollover-v1-live".into(),
                )),
                mode: TransitionMode::Planned,
            },
            &info(),
            &env,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "rolled over build-rollover-v1-live\n"
        );
        assert!(terminator.terminated().is_empty());
        assert_eq!(launcher.launches(), 0);
    }

    #[test]
    fn run_routes_stop_to_the_record_backed_stop() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        // No record yet: stop reports there is nothing to stop.
        assert_eq!(
            run_line(stop(), &store),
            "usagi v0.1.0: daemon not running\n"
        );
    }

    /// A daemon that still owns a runtime is neither stopped nor replaced by
    /// the ordinary verbs — the guard lives on the one path both take.
    #[test]
    fn run_refuses_both_transitions_while_a_runtime_is_live() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        store.save(&DaemonRecord::new(4321)).unwrap();
        let (probe, terminator, shutdown, sleeper) = (
            FixedProbe(true),
            RecordingTerminator::default(),
            ImmediateShutdown,
            NoopSleeper,
        );
        let launcher = TestLauncher::registering(&store, 5555);
        let ready = NoopReady;
        let env = DaemonEnv {
            authority: &FakeAuthority::default(),
            standby_endpoint: &NoopStandbyEndpoint,
            standby_authority: &CountingStandbyAuthority::default(),
            store: &store,
            probe: &probe,
            terminator: &terminator,
            ready: &ready,
            shutdown: &shutdown,
            launcher: &launcher,
            sleeper: &sleeper,
            lock: &FakeLock::Acquired,
            workspace: &FakeWorkspaceFence::Acquired,
            pid: 4321,
            census: &Owning(1),
            seamless: Some(SeamlessRefusal::NoGenerationRegistry),
            rollover: &NoopRollover,
        };
        for command in [stop(), replace()] {
            let error = run(&mut Vec::new(), command, &info(), &env).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        }
        assert!(terminator.terminated().is_empty());
        assert_eq!(launcher.launches(), 0);
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
            stop(),
            DaemonCommand::Start,
            replace(),
        ] {
            let store = DaemonRecordStore::new(InMemoryRecordFile::with("not json"));
            let launcher = TestLauncher::idle(&store);
            let ready = NoopReady;
            let env = DaemonEnv {
                authority: &FakeAuthority::default(),
                standby_endpoint: &NoopStandbyEndpoint,
                standby_authority: &CountingStandbyAuthority::default(),
                store: &store,
                probe: &probe,
                terminator: &terminator,
                ready: &ready,
                shutdown: &shutdown,
                launcher: &launcher,
                sleeper: &sleeper,
                lock: &FakeLock::Acquired,
                workspace: &FakeWorkspaceFence::Acquired,
                pid: 4321,
                census: &Owning(0),
                seamless: Some(SeamlessRefusal::NoGenerationRegistry),
                rollover: &NoopRollover,
            };
            let mut buf = Vec::new();
            assert!(run(&mut buf, command, &info(), &env).is_err());
        }
    }
}

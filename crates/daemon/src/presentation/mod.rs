//! daemon サーバの presentation 層。daemon 面の entry point と、IPC リクエストの
//! dispatch・応答整形を持ち、ロジックは usagi-core の usecase（監視・store 系）と
//! 本クレートの daemon 専用 usecase（`crate::usecase`）へ委譲する。
//! 実 socket・PTY は合成ルートが束ね、この層は注入された入出力に対して純粋に振る舞う。
//! v2 では必要になった時点で端点を追加する。

use std::io::Write;

use usagi_core::domain::AppInfo;
use usagi_core::infrastructure::daemon::{DaemonRecordStore, LivenessProbe, RecordFile};

use crate::usecase;

/// daemon 面の entry point。合成ルートが `usagi daemon` で dispatch する interface で、
/// `usagi daemon` に続くサブコマンド（無しは `None`）を解決し、その結果 1 行を注入された
/// `out` へ書き出す。この層は解決と書き出しの配線に徹し、独自のビジネスロジックは持たない。
///
/// 状態問い合わせの `status` だけは、注入された `store`（`daemon.json` の読取）と `probe`
/// （pid 生存判定）を使う実 IO を伴うため [`usecase::status::report`] へ振り分ける。
/// それ以外のサブコマンドは純粋な [`usecase::Command`]（[`usecase::interpret`] が解決）
/// の結果を書き出す。`store` / `probe` の本物（ファイル・signal）は合成ルートが束ねる。
///
/// 常駐ループ・IPC 待ち受けは `serve` コマンドの実処理として、`start` / `stop` / `restart`
/// の実処理は各スタブに、今後足していく。
///
/// # Errors
///
/// `status` のレコード読取に失敗した場合、または `out` への書き込みに失敗した場合、
/// そのエラーを返す。
pub fn run<W: Write, F: RecordFile, P: LivenessProbe>(
    out: &mut W,
    subcommand: Option<&str>,
    info: &AppInfo,
    store: &DaemonRecordStore<F>,
    probe: &P,
) -> std::io::Result<()> {
    let line = match subcommand {
        Some("status") => usecase::status::report(store, probe, info)?,
        other => usecase::interpret(other).execute(info),
    };
    writeln!(out, "{line}")
}

#[cfg(test)]
mod tests {
    use super::run;
    use crate::test_support::{FixedProbe, InMemoryRecordFile};
    use usagi_core::domain::AppInfo;
    use usagi_core::domain::daemon::DaemonRecord;
    use usagi_core::infrastructure::daemon::DaemonRecordStore;

    fn info() -> AppInfo {
        AppInfo {
            name: "usagi",
            version: "0.1.0",
        }
    }

    fn run_line(subcommand: Option<&str>, store: &DaemonRecordStore<InMemoryRecordFile>) -> String {
        let mut buf = Vec::new();
        run(&mut buf, subcommand, &info(), store, &FixedProbe(true)).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn run_serves_on_none_and_serve() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        for subcommand in [None, Some("serve")] {
            assert_eq!(run_line(subcommand, &store), "usagi v0.1.0 daemon ready\n");
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
    fn run_reports_control_plane_stubs() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        for verb in ["start", "stop", "restart"] {
            assert_eq!(
                run_line(Some(verb), &store),
                format!("usagi v0.1.0: daemon {verb} is not yet implemented\n")
            );
        }
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
    fn run_propagates_status_read_error() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::with("not json"));
        let mut buf = Vec::new();
        assert!(run(&mut buf, Some("status"), &info(), &store, &FixedProbe(true)).is_err());
    }
}

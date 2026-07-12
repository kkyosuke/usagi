//! daemon サーバの presentation 層。daemon 面の entry point と、IPC リクエストの
//! dispatch・応答整形を持ち、ロジックは usagi-core の usecase（監視・store 系）と
//! 本クレートの daemon 専用 usecase（`crate::usecase`）へ委譲する。
//! 実 socket・PTY は合成ルートが束ね、この層は注入された入出力に対して純粋に振る舞う。
//! v2 では必要になった時点で端点を追加する。

use std::io::Write;

use usagi_core::domain::AppInfo;

use crate::usecase;

/// daemon 面の entry point。合成ルートが `usagi daemon` で dispatch する interface で、
/// `usagi daemon` に続くサブコマンド（無しは `None`）を `crate::usecase::interpret` で
/// [`crate::usecase::Command`] へ解決し、その `execute` の結果を注入された `out` へ書き出す。
/// この層は解決と書き出しの配線に徹し、独自のビジネスロジックは持たない。
///
/// サブコマンドの解釈以外の引数（socket パス・設定など）はまだ最小限で、実装が
/// 進んだ時点で追加する。常駐ループ・IPC 待ち受けは `serve` コマンドの実処理として足していく。
///
/// # Errors
///
/// `out` への書き込みに失敗した場合、そのエラーを返す。
pub fn run(out: &mut impl Write, subcommand: Option<&str>, info: &AppInfo) -> std::io::Result<()> {
    let command = usecase::interpret(subcommand);
    writeln!(out, "{}", command.execute(info))
}

#[cfg(test)]
mod tests {
    use super::run;
    use usagi_core::domain::AppInfo;

    fn info() -> AppInfo {
        AppInfo {
            name: "usagi",
            version: "0.1.0",
        }
    }

    #[test]
    fn run_serves_on_none_and_serve() {
        for subcommand in [None, Some("serve")] {
            let mut buf = Vec::new();
            run(&mut buf, subcommand, &info()).unwrap();
            assert_eq!(
                String::from_utf8(buf).unwrap(),
                "usagi v0.1.0 daemon ready\n"
            );
        }
    }

    #[test]
    fn run_reports_unknown_subcommand() {
        let mut buf = Vec::new();
        run(&mut buf, Some("bogus"), &info()).unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "usagi v0.1.0: unknown daemon subcommand `bogus`\n"
        );
    }
}

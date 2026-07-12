//! daemon サーバの presentation 層。daemon 面の entry point と、IPC リクエストの
//! dispatch・応答整形を持ち、ロジックは usagi-core の usecase（監視・store 系）と
//! 本クレートの daemon 専用 usecase（`crate::usecase`）へ委譲する。
//! 実 socket・PTY は合成ルートが束ね、この層は注入された入出力に対して純粋に振る舞う。
//! v2 では必要になった時点で端点を追加する。

use std::io::Write;

use usagi_core::domain::AppInfo;

use crate::usecase;

/// daemon 面の entry point。合成ルートが `usagi daemon` で dispatch する interface で、
/// アプリケーションロジックは `crate::usecase` へ委譲し、この層は結果を注入された
/// `out` へ書き出すだけに徹する（独自のビジネスロジックを持たない）。
///
/// 引数はまだ最小限（socket パス・設定などは未定で、実装が進んだ時点で追加する）。
/// 現状は起動アナウンスを出す最小の入口で、常駐ループ・IPC 待ち受けはここに足していく。
///
/// # Errors
///
/// `out` への書き込みに失敗した場合、そのエラーを返す。
pub fn run(out: &mut impl Write, info: &AppInfo) -> std::io::Result<()> {
    writeln!(out, "{}", usecase::startup_announcement(info))
}

#[cfg(test)]
mod tests {
    use super::run;
    use usagi_core::domain::AppInfo;

    #[test]
    fn run_writes_startup_announcement() {
        let info = AppInfo {
            name: "usagi",
            version: "0.1.0",
        };
        let mut buf = Vec::new();
        run(&mut buf, &info).unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "usagi v0.1.0 daemon ready\n"
        );
    }
}

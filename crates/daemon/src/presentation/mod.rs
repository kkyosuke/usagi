//! daemon サーバの presentation 層。IPC リクエストの dispatch と応答整形を持ち、
//! ロジックは usagi-core の usecase（監視・store 系）と本クレートの daemon 専用
//! usecase へ委譲する。実 socket・PTY は合成ルートが束ね、この層は注入された
//! 入出力に対して純粋に振る舞う。v2 では必要になった時点で端点を追加する。

use std::io::Write;

use usagi_core::domain::AppInfo;

/// daemon の起動完了を示す ready 行を `out` に書き出す。
///
/// # Errors
///
/// `out` への書き込みに失敗した場合、そのエラーを返す。
pub fn write_ready_line(out: &mut impl Write, info: &AppInfo) -> std::io::Result<()> {
    writeln!(out, "{} daemon ready", info.describe())
}

#[cfg(test)]
mod tests {
    use super::write_ready_line;
    use usagi_core::domain::AppInfo;

    #[test]
    fn write_ready_line_writes_description_and_marker() {
        let info = AppInfo {
            name: "usagi",
            version: "0.1.0",
        };
        let mut buf = Vec::new();
        write_ready_line(&mut buf, &info).unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "usagi v0.1.0 daemon ready\n"
        );
    }
}

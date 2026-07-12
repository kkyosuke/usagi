//! 人間向け CLI サブコマンドの presentation。引数解析・dispatch・結果整形だけを持ち、
//! ロジックは usagi-core の usecase（store 系）と daemon への IPC（session 系）へ
//! 委譲する。個々のサブコマンドハンドラは `commands` に置き、引数解析・整形が肥大したら
//! 専用モジュールへ分割する。v2 では必要になった時点でサブコマンドを追加する。

pub mod commands;

use std::io::Write;

use usagi_core::domain::AppInfo;

/// 未実装のサブコマンドに対する案内行を `out` に書き出す。
///
/// 合成ルートが dispatch した先のサブコマンドがまだ v2 に存在しないときに使う。
///
/// # Errors
///
/// `out` への書き込みに失敗した場合、そのエラーを返す。
pub fn write_unknown_command(
    out: &mut impl Write,
    info: &AppInfo,
    command: &str,
) -> std::io::Result<()> {
    writeln!(out, "{}: unknown command `{command}`", info.describe())
}

#[cfg(test)]
mod tests {
    use super::write_unknown_command;
    use usagi_core::domain::AppInfo;

    #[test]
    fn write_unknown_command_names_the_command() {
        let info = AppInfo {
            name: "usagi",
            version: "0.1.0",
        };
        let mut buf = Vec::new();
        write_unknown_command(&mut buf, &info, "status").unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "usagi v0.1.0: unknown command `status`\n"
        );
    }
}

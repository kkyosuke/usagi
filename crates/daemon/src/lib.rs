//! usagi-daemon — 常駐プロセス（`usagi daemon`）のサーバ面クレート。
//!
//! agent / シェルの PTY 所有・セッション監視・委譲 queue の消化
//! （document/proposals/02-daemon.md）をここに実装する。
//! usagi-core にのみ依存し、usagi-tui には依存しない（TUI との通信は
//! usagi-core の IPC プロトコル型を介して行う）。
//! 実 IO は行わず、入出力は呼び出し側（合成ルート）から注入する。

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

//! MCP サーバ（`usagi mcp`）の presentation。stdio JSON-RPC の解釈と dispatch を持ち、
//! ロジックは usagi-core の usecase（store 系）と daemon への IPC（session 系）へ
//! 委譲する。個々の tool アダプタは `tools` に置き、JSON-RPC フレーミングが肥大したら
//! 専用モジュールへ分割する。v2 では必要になった時点で tool を追加する。

pub mod tools;

use std::io::Write;

use usagi_core::domain::AppInfo;

/// MCP 面の起動を示す ready 行を `out` に書き出す。
///
/// # Errors
///
/// `out` への書き込みに失敗した場合、そのエラーを返す。
pub fn write_ready_line(out: &mut impl Write, info: &AppInfo) -> std::io::Result<()> {
    writeln!(out, "{} mcp ready", info.describe())
}

#[cfg(test)]
mod tests {
    use super::write_ready_line;
    use usagi_core::domain::AppInfo;

    #[test]
    fn write_ready_line_marks_mcp_surface() {
        let info = AppInfo {
            name: "usagi",
            version: "0.1.0",
        };
        let mut buf = Vec::new();
        write_ready_line(&mut buf, &info).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "usagi v0.1.0 mcp ready\n");
    }
}

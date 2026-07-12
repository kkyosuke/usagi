//! presentation 層。CLI / TUI の入出力表現を組み立てる。
//! 実 IO は行わず、出力先は呼び出し側（合成ルート）から注入する。

use std::io::Write;

use crate::domain::AppInfo;

/// 起動バナーを `out` に書き出す。
///
/// # Errors
///
/// `out` への書き込みに失敗した場合、そのエラーを返す。
pub fn write_banner(out: &mut impl Write, info: &AppInfo) -> std::io::Result<()> {
    writeln!(out, "{}", info.describe())
}

#[cfg(test)]
mod tests {
    use super::write_banner;
    use crate::domain::AppInfo;

    #[test]
    fn write_banner_writes_description_line() {
        let info = AppInfo {
            name: "usagi",
            version: "0.1.0",
        };
        let mut buf = Vec::new();
        write_banner(&mut buf, &info).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "usagi v0.1.0\n");
    }
}

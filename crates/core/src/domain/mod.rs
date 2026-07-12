//! domain 層。ビジネスルールとエンティティを置く。
//! 他層・他クレート・外部クレートに依存しない。

/// アプリケーションの自己記述。バージョン表示などで使う。
///
/// `name` / `version` は配布バイナリの Cargo メタデータを指すため、
/// 合成ルート（ルートパッケージ）が `env!` で埋めて構築する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppInfo {
    /// バイナリ名。
    pub name: &'static str,
    /// `SemVer` 形式のバージョン文字列。
    pub version: &'static str,
}

impl AppInfo {
    /// `name v<version>` 形式の一行表現を返す。
    #[must_use]
    pub fn describe(&self) -> String {
        format!("{} v{}", self.name, self.version)
    }
}

#[cfg(test)]
mod tests {
    use super::AppInfo;

    #[test]
    fn describe_formats_name_and_version() {
        let info = AppInfo {
            name: "usagi",
            version: "0.1.0",
        };
        assert_eq!(info.describe(), "usagi v0.1.0");
        // derive された Clone / PartialEq / Debug も計測対象のため、ここで実行する。
        assert_eq!(info.clone(), info);
        assert!(format!("{info:?}").contains("usagi"));
    }
}

//! domain 層。ビジネスルールとエンティティを置く。
//! 他層・他 usagi クレートには依存しない。時刻（`chrono`）と (de)serialize
//! 語彙（`serde`）だけは基盤語彙として使う（詳細は
//! [`document/02-architecture.md`](../../../../document/02-architecture.md) の依存ルール）。

pub mod agent;
pub mod daemon;
pub mod frontmatter;
pub mod id;
pub mod issue;
pub mod memory;
pub mod note;
pub mod pr_inventory;
pub mod pullrequest;
pub mod recent;
pub mod session;
pub mod session_lifecycle;
pub mod settings;
pub mod supervisor;
pub mod terminal_launch;
pub mod trace;
pub mod workspace;
pub mod workspace_state;

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

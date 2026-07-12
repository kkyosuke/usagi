//! usecase 層。domain を組み合わせてアプリケーションの操作を表す。

use crate::domain::AppInfo;

/// ビルド時の Cargo メタデータからこのバイナリの [`AppInfo`] を構築する。
#[must_use]
pub fn app_info() -> AppInfo {
    AppInfo {
        name: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
    }
}

#[cfg(test)]
mod tests {
    use super::app_info;

    #[test]
    fn app_info_reflects_cargo_metadata() {
        let info = app_info();
        assert_eq!(info.name, env!("CARGO_PKG_NAME"));
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
    }
}

//! daemon 専用の usecase 層。TUI と共有するロジック（セッション作成・設定解決など）は
//! usagi-core の usecase に置き、ここには daemon だけが駆動するロジックを置く
//! （daemon 起動・セッション監視ティック・委譲 queue の消化＝autostart・
//! waiting/done 通知の調停・孤児端末 adopt の判定）。domain は usagi-core を再利用し、
//! 実 IO は注入する。v2 では必要になった時点で実装を追加する。
//!
//! 制御プレーンの verb はすべて実 IO（store 読取・生存判定・signal・常駐待受・プロセス
//! 起動）を要するため、各 usecase モジュール（[`serve`] / [`start`] / [`status`] / [`stop`] /
//! [`restart`]）が担い、[`crate::presentation::run`] が振り分ける。認識できない
//! サブコマンドだけは実 IO を伴わないので [`unknown_subcommand`] が案内 1 行を組み立てる。

use usagi_core::domain::AppInfo;

pub mod control;
pub mod restart;
pub mod serve;
pub mod start;
pub mod status;
pub mod stop;

/// 認識できない `usagi daemon <subcommand>` に対する案内 1 行を返す。
///
/// 実 IO を要する制御プレーン verb（`serve`（無指定含む）/ `start` / `status` / `stop` /
/// `restart`）は [`crate::presentation::run`] が各 usecase へ先に振り分けるため、ここには
/// 認識できないサブコマンドだけが渡る。
#[must_use]
pub fn unknown_subcommand(info: &AppInfo, subcommand: &str) -> String {
    format!(
        "{}: unknown daemon subcommand `{subcommand}`",
        info.describe()
    )
}

#[cfg(test)]
mod tests {
    use super::unknown_subcommand;
    use usagi_core::domain::AppInfo;

    fn info() -> AppInfo {
        AppInfo {
            name: "usagi",
            version: "0.1.0",
        }
    }

    #[test]
    fn unknown_subcommand_builds_a_guidance_line() {
        assert_eq!(
            unknown_subcommand(&info(), "bogus"),
            "usagi v0.1.0: unknown daemon subcommand `bogus`"
        );
    }
}

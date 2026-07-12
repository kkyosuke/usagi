//! daemon 専用の usecase 層。TUI と共有するロジック（セッション作成・設定解決など）は
//! usagi-core の usecase に置き、ここには daemon だけが駆動するロジックを置く
//! （daemon 起動・セッション監視ティック・委譲 queue の消化＝autostart・
//! waiting/done 通知の調停・孤児端末 adopt の判定）。domain は usagi-core を再利用し、
//! 実 IO は注入する。v2 では必要になった時点で実装を追加する。

use usagi_core::domain::AppInfo;

/// daemon 起動 usecase。実 IO を持たない純粋なアプリケーションロジックで、
/// entry point（`crate::presentation::run`）が注入された writer へ書き出す
/// 起動アナウンスを組み立てて返す。監視ティック・queue 消化など常駐処理の本体は、
/// entry point がこの層を呼ぶ形で段階的にここへ足していく。
///
/// 引数は現状 `AppInfo` のみ（socket パス・設定などは未定で、実装が進んだ時点で追加）。
#[must_use]
pub fn startup_announcement(info: &AppInfo) -> String {
    format!("{} daemon ready", info.describe())
}

#[cfg(test)]
mod tests {
    use super::startup_announcement;
    use usagi_core::domain::AppInfo;

    #[test]
    fn startup_announcement_marks_daemon_ready() {
        let info = AppInfo {
            name: "usagi",
            version: "0.1.0",
        };
        assert_eq!(startup_announcement(&info), "usagi v0.1.0 daemon ready");
    }
}

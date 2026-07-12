//! daemon 専用の usecase 層。TUI と共有するロジック（セッション作成・設定解決など）は
//! usagi-core の usecase に置き、ここには daemon だけが駆動するロジックを置く
//! （daemon 起動・セッション監視ティック・委譲 queue の消化＝autostart・
//! waiting/done 通知の調停・孤児端末 adopt の判定）。domain は usagi-core を再利用し、
//! 実 IO は注入する。v2 では必要になった時点で実装を追加する。

use usagi_core::domain::AppInfo;

/// `usagi daemon <subcommand>` の 1 つのサブコマンドを表すコマンド（コマンドパターン）。
///
/// 各コマンドは実 IO を持たない純粋なアプリケーション操作で、実行結果として
/// 標準出力へ出す 1 行を組み立てて返す。実際の書き出し・常駐ループ・プロセス
/// 起動などの実 IO は presentation / 合成ルートが担う。制御プレーンの verb
/// （`start` / `stop` / `status` / `restart`）は現状 IF（コマンド型と routing）だけを
/// 用意したスタブで、daemon レコード store・単一インスタンスロック・プロセス管理が
/// 入った段階で各コマンドの `execute` に実処理を入れる（注入した依存を引数に取る形で
/// この trait を拡張する。[document/proposals/02-daemon.md] の Step 1）。
///
/// [`interpret`] がサブコマンド文字列を対応するコマンドへ解決するファクトリである。
pub trait Command {
    /// コマンドを実行し、標準出力へ出す 1 行を返す。
    fn execute(&self, info: &AppInfo) -> String;
}

/// 常駐ループを前景で走らせる `serve`（サブコマンド無しと同義。`serve` は隠しサブコマンド）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Serve;

impl Command for Serve {
    fn execute(&self, info: &AppInfo) -> String {
        format!("{} daemon ready", info.describe())
    }
}

/// 認識できないサブコマンド。案内を返して終える。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unknown {
    subcommand: String,
}

impl Unknown {
    /// 認識できなかったサブコマンド名から [`Unknown`] を作る。
    pub fn new(subcommand: impl Into<String>) -> Self {
        Self {
            subcommand: subcommand.into(),
        }
    }
}

impl Command for Unknown {
    fn execute(&self, info: &AppInfo) -> String {
        format!(
            "{}: unknown daemon subcommand `{}`",
            info.describe(),
            self.subcommand
        )
    }
}

/// daemon を detached 起動して register する `start`。
pub struct Start;

impl Command for Start {
    fn execute(&self, info: &AppInfo) -> String {
        not_yet_implemented(info, "start")
    }
}

/// 稼働中の daemon に stop マーカーを立てて止める `stop`。
pub struct Stop;

impl Command for Stop {
    fn execute(&self, info: &AppInfo) -> String {
        not_yet_implemented(info, "stop")
    }
}

/// daemon の状態（alive / stale / absent）を報告する `status`。
pub struct Status;

impl Command for Status {
    fn execute(&self, info: &AppInfo) -> String {
        not_yet_implemented(info, "status")
    }
}

/// 稼働中の daemon を止めてから起動し直す `restart`（`stop` → `start` の複合）。
pub struct Restart;

impl Command for Restart {
    fn execute(&self, info: &AppInfo) -> String {
        not_yet_implemented(info, "restart")
    }
}

/// 制御プレーンの verb のうち、まだ実処理（daemon レコード store・単一インスタンス
/// ロック・プロセス管理）が入っていないスタブが返す案内。IF（コマンド型と routing）
/// だけを先に用意し、実装が入った時点で各コマンドの `execute` を差し替える。
fn not_yet_implemented(info: &AppInfo, verb: &str) -> String {
    format!("{}: daemon {verb} is not yet implemented", info.describe())
}

/// `usagi daemon` に続くサブコマンド（無しは `None`）を対応する [`Command`] へ解決する。
///
/// サブコマンド無しと `serve` は同じ前景実行（[`Serve`]）にまとめ、制御プレーンの verb
/// （`start` / `stop` / `status` / `restart`）はそれぞれの実装型へ、それ以外は [`Unknown`]
/// にする。返り値を動的ディスパッチにして、verb ごとに実装型を増やしても呼び出し側
/// （entry point）を変えずに済むようにする。
#[must_use]
pub fn interpret(subcommand: Option<&str>) -> Box<dyn Command> {
    match subcommand {
        None | Some("serve") => Box::new(Serve),
        Some("start") => Box::new(Start),
        Some("stop") => Box::new(Stop),
        Some("status") => Box::new(Status),
        Some("restart") => Box::new(Restart),
        Some(other) => Box::new(Unknown::new(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, Serve, Unknown, interpret};
    use usagi_core::domain::AppInfo;

    fn info() -> AppInfo {
        AppInfo {
            name: "usagi",
            version: "0.1.0",
        }
    }

    #[test]
    fn serve_executes_to_ready_line() {
        assert_eq!(Serve.execute(&info()), "usagi v0.1.0 daemon ready");
        // derive された Clone / Copy / Debug も計測対象のため、ここで実行する。
        let serve = Serve;
        assert_eq!({ serve }, serve);
        assert!(format!("{serve:?}").contains("Serve"));
    }

    #[test]
    fn unknown_executes_to_guidance_line() {
        let unknown = Unknown::new("bogus");
        assert_eq!(
            unknown.execute(&info()),
            "usagi v0.1.0: unknown daemon subcommand `bogus`"
        );
        // derive された Clone / Debug も計測対象のため、ここで実行する。
        assert_eq!(unknown.clone(), unknown);
        assert!(format!("{unknown:?}").contains("bogus"));
    }

    #[test]
    fn interpret_resolves_none_and_serve_to_serve() {
        assert_eq!(
            interpret(None).execute(&info()),
            "usagi v0.1.0 daemon ready"
        );
        assert_eq!(
            interpret(Some("serve")).execute(&info()),
            "usagi v0.1.0 daemon ready"
        );
    }

    #[test]
    fn interpret_resolves_control_plane_verbs_to_not_yet_implemented_stubs() {
        for verb in ["start", "stop", "status", "restart"] {
            assert_eq!(
                interpret(Some(verb)).execute(&info()),
                format!("usagi v0.1.0: daemon {verb} is not yet implemented")
            );
        }
    }

    #[test]
    fn interpret_resolves_other_to_unknown() {
        assert_eq!(
            interpret(Some("bogus")).execute(&info()),
            "usagi v0.1.0: unknown daemon subcommand `bogus`"
        );
    }
}

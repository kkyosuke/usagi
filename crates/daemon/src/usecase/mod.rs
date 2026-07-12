//! daemon 専用の usecase 層。TUI と共有するロジック（セッション作成・設定解決など）は
//! usagi-core の usecase に置き、ここには daemon だけが駆動するロジックを置く
//! （daemon 起動・セッション監視ティック・委譲 queue の消化＝autostart・
//! waiting/done 通知の調停・孤児端末 adopt の判定）。domain は usagi-core を再利用し、
//! 実 IO は注入する。v2 では必要になった時点で実装を追加する。

use usagi_core::domain::AppInfo;

pub mod serve;
pub mod status;
pub mod stop;

/// `usagi daemon <subcommand>` の 1 つのサブコマンドを表すコマンド（コマンドパターン）。
///
/// 各コマンドは実 IO を持たない純粋なアプリケーション操作で、実行結果として
/// 標準出力へ出す 1 行を組み立てて返す。実際の書き出し・常駐ループ・プロセス
/// 起動などの実 IO は presentation / 合成ルートが担う。まだ実処理（プロセス起動）が
/// 入っていない制御プレーンの verb（`start` / `restart`）は IF（コマンド型と routing）だけを
/// 用意したスタブで、実装が入った段階で各コマンドの `execute` に実処理を入れる。
///
/// 実 IO（store 読取・生存判定・signal・常駐待受）を要する `serve` / `status` / `stop` は、
/// この純粋な trait ではなく [`serve::serve`] / [`status::report`] / [`stop::stop`] が担う
/// （[`crate::presentation::run`] が振り分ける）。
///
/// [`interpret`] がサブコマンド文字列を対応するコマンドへ解決するファクトリである。
pub trait Command {
    /// コマンドを実行し、標準出力へ出す 1 行を返す。
    fn execute(&self, info: &AppInfo) -> String;
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

/// 純粋な [`Command`]（実 IO を伴わないスタブ verb）へサブコマンド文字列を解決する。
///
/// 制御プレーンのスタブ verb（`start` / `restart`）はそれぞれの実装型へ、それ以外は
/// [`Unknown`] にする。実 IO を要する `serve`（無指定含む）/ `status` / `stop` は
/// [`crate::presentation::run`] が [`serve::serve`] / [`status::report`] / [`stop::stop`] へ
/// 先に振り分けるため、ここには渡らない。返り値を動的ディスパッチにして、verb ごとに実装型を
/// 増やしても呼び出し側を変えずに済むようにする。
#[must_use]
pub fn interpret(subcommand: &str) -> Box<dyn Command> {
    match subcommand {
        "start" => Box::new(Start),
        "restart" => Box::new(Restart),
        other => Box::new(Unknown::new(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, Unknown, interpret};
    use usagi_core::domain::AppInfo;

    fn info() -> AppInfo {
        AppInfo {
            name: "usagi",
            version: "0.1.0",
        }
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
    fn interpret_resolves_control_plane_verbs_to_not_yet_implemented_stubs() {
        for verb in ["start", "restart"] {
            assert_eq!(
                interpret(verb).execute(&info()),
                format!("usagi v0.1.0: daemon {verb} is not yet implemented")
            );
        }
    }

    #[test]
    fn interpret_resolves_other_to_unknown() {
        assert_eq!(
            interpret("bogus").execute(&info()),
            "usagi v0.1.0: unknown daemon subcommand `bogus`"
        );
    }
}

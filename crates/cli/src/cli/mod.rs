//! 人間向け CLI サブコマンドの presentation。ここには **コマンド面の枠** だけを置く:
//! clap による引数解析ツリー（どんなコマンド・オプションがあるか）と、`Run`
//! トレイトによる多態 dispatch である。各コマンドの中身は今後ハンドラ（`commands`）に
//! 実装していく。
//!
//! ここに置くのは **ターミナルから `usagi <cmd>` で叩く人間向けコマンド** だけである。
//! エージェント向けの issue / memory 操作は MCP 面（`crate::mcp`）が受け持ち、CLI には置かない。
//!
//! TUI を開くコマンドは [`RunOutcome::LaunchTui`] を返し、合成ルートが TUI 面へ接続する。
//! それ以外のコマンドは出力後に [`RunOutcome::Exit`] を返す。

pub mod commands;
pub mod hooks;

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};

/// CLI が合成ルートへ依頼する TUI の起動画面。
///
/// `usagi-cli` は `usagi-tui` に依存せず、この入口面の要求だけを返す。合成ルートが
/// TUI 面の画面型へ変換することで、面クレート間の依存を作らずに接続する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiRequest {
    /// Welcome 画面を開く。
    Welcome,
    /// workspace 画面を開く。`path` 省略時はカレントディレクトリを使う。
    Workspace {
        /// 開くディレクトリ。
        path: Option<PathBuf>,
    },
    /// Config 画面を開く。
    Config,
    /// Doctor 画面を開く。
    Doctor,
}

/// CLI の解析・ハンドラ実行結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    /// CLI で処理が完了したときのプロセス終了コード。
    Exit(i32),
    /// 合成ルートに TUI の起動を依頼する。
    LaunchTui(TuiRequest),
}

/// 実行可能な CLI サブコマンドの共通インターフェース。
///
/// clap が解釈した各コマンドは、自分の実行方法を知る型（`commands` のハンドラ）に
/// 変換され、dispatch は「型ごとに分岐する巨大な match」ではなく一様な `run` 呼び出しに
/// なる。出力先（`out`）は注入されるため、ハンドラは実 IO なしでユニットテストできる。
pub trait Run {
    /// サブコマンドを実行し、CLI 完了または TUI 起動要求を返す。
    ///
    /// # Errors
    ///
    /// `out` への書き込みに失敗した場合、そのエラーを返す。
    fn run(&self, out: &mut dyn Write) -> io::Result<RunOutcome>;
}

/// `usagi` の CLI コマンドツリー（`clap` による引数解析の入口）。
///
/// 第 1 引数で面を選ぶ合成ルート（ルート `main.rs`）は、TUI（引数なし）・daemon
/// （`usagi daemon`）・MCP（`usagi mcp`）を先に振り分け、それ以外のサブコマンドを
/// この parser に渡す。したがってここには **人間向け CLI サブコマンド** だけを定義する。
#[derive(Debug, Parser)]
#[command(
    name = "usagi",
    about = "AI エージェントのワークフローを管理する TUI/CLI",
    // 構造説明の doc コメントを `--help` の long_about に流用しない（開発者向けのまま残す）。
    long_about = None
)]
// version は derive で固定しない。配布 version はルートパッケージだけが持つ
// （document/02-architecture.md）ため、`--version` の値は合成ルートから run() に注入し、
// clap コマンドに `.version()` で載せる（cli クレートの 0.0.0 を出さない）。
pub struct Cli {
    /// 実行するサブコマンド。
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// CLI サブコマンドの一覧。各バリアントがそのコマンドの受け付けるオプションを型として表す。
/// `help` は clap が自動で用意する。
///
/// 大半は人間向けだが、末尾の 2 つ（`AgentPhase` / `GuardWorkspace`）は usagi が
/// エージェント起動時に Claude のフックへ配線する**内部コマンド**で、`--help` には出さない
/// （`hide = true`）。人手で叩くものではない。
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Welcome TUI を開く（引数なし起動の互換 alias）
    #[command(hide = true)]
    Hop,
    /// ディレクトリをプロジェクトとして登録して TUI で開く
    Open {
        /// 開くディレクトリ（省略時はカレントディレクトリ）
        path: Option<PathBuf>,
    },
    /// 設定を編集する（TUI の Config を開く）
    Config,
    /// 必要ツールの導入状況を診断する（TUI の Doctor を開く）
    Doctor,
    /// 最新版があるか確認する
    Update,
    /// 指定シェルの補完スクリプトを標準出力に印字する（Tab 補完を有効化する）
    Completion {
        /// 補完スクリプトを生成する対象シェル
        shell: Shell,
    },
    /// バージョンを表示する
    Version,
    /// （ヘルプ非表示・内部）エージェントのライフサイクル phase を記録する（Stop フックが呼ぶ）
    #[command(hide = true)]
    AgentPhase {
        /// フックが報告する phase（例: `ended`）
        phase: String,
    },
    /// （ヘルプ非表示・内部）worktree の外へ出るツール呼び出しを拒否する（`PreToolUse` フックが呼ぶ）
    #[command(hide = true)]
    GuardWorkspace,
}

/// 補完スクリプトを生成する対象シェル。
///
/// 生成そのものは未実装のため、いまはシェル種別の受け付けだけを表す
/// （実装時に `clap_complete` を導入する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    Powershell,
    Elvish,
}

impl Command {
    /// 解釈済みのコマンドを、その実行方法を知るハンドラ（`Run`）に変換する。
    ///
    /// dispatch はこの 1 か所の対応付けに集約し、実行自体は `Run::run` の一様な
    /// 呼び出しになる。`version` は入口だけが持つ配布 version を渡す（他コマンドは使わない）。
    #[must_use]
    pub fn into_handler(self, version: &str) -> Box<dyn Run> {
        use commands as h;
        match self {
            Command::Hop => Box::new(h::Hop),
            Command::Open { path } => Box::new(h::Open { path }),
            Command::Config => Box::new(h::Config),
            Command::Doctor => Box::new(h::Doctor),
            Command::Update => Box::new(h::Update),
            Command::Completion { shell } => Box::new(h::Completion { shell }),
            Command::Version => Box::new(h::Version {
                version: version.to_owned(),
            }),
            // エージェント統合フックは commands/ ではなく hooks/ に置く。
            Command::AgentPhase { phase } => Box::new(hooks::AgentPhase { phase }),
            Command::GuardWorkspace => Box::new(hooks::GuardWorkspace),
        }
    }
}

/// コマンドを dispatch してハンドラを実行し、結果と出力文字列を得るテストヘルパ。
/// `commands` / `hooks` 双方のハンドラテストから使い、`Command::into_handler` の各アームを被覆する。
#[cfg(test)]
pub(crate) fn execute(command: Command) -> (RunOutcome, String) {
    let mut out = Vec::new();
    let outcome = command.into_handler("9.9.9").run(&mut out).unwrap();
    (outcome, String::from_utf8(out).unwrap())
}

/// CLI 面のエントリポイント。`args`（プログラム名を含む argv）を解析し、
/// 対応するハンドラを実行して、CLI 完了または TUI 起動要求を返す。
///
/// 出力は注入された `out` / `err` に書く（合成ルートが実 stdout / stderr を束ねる）。
/// clap の `--help` / `--version` は `out` に、使い方エラーは `err` に出す慣習に従う。
///
/// `args` は `OsString` の具体型で受ける（ジェネリックにすると呼び出し側の
/// イテレータ型ごとに単相化が増え、テストで到達しない実体がカバレッジを下げるため）。
/// `version` は `--version` と `version` サブコマンドに載せる配布 version（合成ルートが注入する）。
/// 合成ルートは `std::env::args_os().collect()` と自身の version を渡す。
///
/// # Errors
///
/// `out` / `err` への書き込み、またはハンドラの実行に失敗した場合、そのエラーを返す。
///
/// # Panics
///
/// 解析済み `ArgMatches` を `Cli` に戻す変換に失敗した場合に panic する。matches は
/// 直前に `Cli::command()` から生成したコマンド定義で解析したものなので、実際には起きない。
pub fn run(
    args: Vec<OsString>,
    version: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> io::Result<RunOutcome> {
    let mut command = Cli::command().version(version.to_owned());
    let matches = match command.clone().try_get_matches_from(args) {
        Ok(matches) => matches,
        Err(e) => {
            // clap の --help / --version は stdout、使い方エラーは stderr に出す慣習に従う。
            let rendered = e.render();
            if e.use_stderr() {
                write!(err, "{rendered}")?;
            } else {
                write!(out, "{rendered}")?;
            }
            return Ok(RunOutcome::Exit(e.exit_code()));
        }
    };
    // matches は Cli::command() から得たものなので、Cli への変換は常に成功する。
    let cli =
        Cli::from_arg_matches(&matches).expect("matches from Cli::command() は Cli に変換できる");
    if let Some(command) = cli.command {
        command.into_handler(version).run(out)
    } else {
        // 引数なしの `usagi` は合成ルートが TUI に振り分けるため、ここに到達するのは
        // グローバルフラグだけが与えられた場合。トップレベルのヘルプを表示する。
        write!(out, "{}", command.render_long_help())?;
        Ok(RunOutcome::Exit(0))
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command, RunOutcome, Shell, TuiRequest, run};
    use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};

    /// `&str` の並びを `run` が受け取る argv（`Vec<OsString>`）に変換する。
    fn argv(tokens: &[&str]) -> Vec<std::ffi::OsString> {
        tokens.iter().map(std::ffi::OsString::from).collect()
    }

    /// オプションなしのサブコマンドを解析できる。
    #[test]
    fn parses_simple_subcommands() {
        assert!(matches!(
            Cli::try_parse_from(["usagi", "hop"]).unwrap().command,
            Some(Command::Hop)
        ));
        assert!(matches!(
            Cli::try_parse_from(["usagi", "config"]).unwrap().command,
            Some(Command::Config)
        ));
        assert!(matches!(
            Cli::try_parse_from(["usagi", "doctor"]).unwrap().command,
            Some(Command::Doctor)
        ));
        assert!(matches!(
            Cli::try_parse_from(["usagi", "update"]).unwrap().command,
            Some(Command::Update)
        ));
        assert!(matches!(
            Cli::try_parse_from(["usagi", "version"]).unwrap().command,
            Some(Command::Version)
        ));
    }

    /// 内部フックコマンド（ヘルプ非表示だが実行可能）も解析できる。
    #[test]
    fn parses_hidden_internal_commands() {
        assert!(matches!(
            Cli::try_parse_from(["usagi", "guard-workspace"])
                .unwrap()
                .command,
            Some(Command::GuardWorkspace)
        ));
        let cli = Cli::try_parse_from(["usagi", "agent-phase", "ended"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::AgentPhase { phase }) if phase == "ended"
        ));
    }

    /// `open` は任意のパス、`completion` は `value_enum` を受け取る。
    #[test]
    fn parses_options() {
        assert!(matches!(
            Cli::try_parse_from(["usagi", "open"]).unwrap().command,
            Some(Command::Open { path: None })
        ));
        assert!(matches!(
            Cli::try_parse_from(["usagi", "open", "/tmp/x"])
                .unwrap()
                .command,
            Some(Command::Open { path: Some(_) })
        ));
        assert!(matches!(
            Cli::try_parse_from(["usagi", "completion", "zsh"])
                .unwrap()
                .command,
            Some(Command::Completion { shell: Shell::Zsh })
        ));
    }

    /// TUI を開くコマンドは、解析済み引数を保った起動要求を返す。
    #[test]
    fn run_returns_tui_requests_without_output() {
        for (tokens, expected) in [
            (&["usagi", "hop"][..], TuiRequest::Welcome),
            (&["usagi", "open"][..], TuiRequest::Workspace { path: None }),
            (
                &["usagi", "open", "/tmp/x"][..],
                TuiRequest::Workspace {
                    path: Some("/tmp/x".into()),
                },
            ),
            (&["usagi", "config"][..], TuiRequest::Config),
            (&["usagi", "doctor"][..], TuiRequest::Doctor),
        ] {
            let mut out = Vec::new();
            let mut err = Vec::new();
            let outcome = run(argv(tokens), "9.9.9", &mut out, &mut err).unwrap();
            assert_eq!(outcome, RunOutcome::LaunchTui(expected));
            assert!(out.is_empty());
            assert!(err.is_empty());
        }
    }

    /// サブコマンドなしはトップレベルのヘルプを `out` に出す。
    #[test]
    fn run_without_subcommand_prints_help() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let outcome = run(argv(&["usagi"]), "9.9.9", &mut out, &mut err).unwrap();
        assert_eq!(outcome, RunOutcome::Exit(0));
        assert!(err.is_empty());
        assert!(String::from_utf8(out).unwrap().contains("Usage"));
    }

    /// `--help` は `out` に出て終了コード 0。
    #[test]
    fn run_help_goes_to_stdout() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let outcome = run(argv(&["usagi", "--help"]), "9.9.9", &mut out, &mut err).unwrap();
        assert_eq!(outcome, RunOutcome::Exit(0));
        assert!(err.is_empty());
        assert!(!out.is_empty());
    }

    /// `--version` フラグと `version` サブコマンドはどちらも注入された配布 version を出す。
    #[test]
    fn run_reports_injected_version() {
        for tokens in [&["usagi", "--version"][..], &["usagi", "version"][..]] {
            let mut out = Vec::new();
            let mut err = Vec::new();
            let outcome = run(argv(tokens), "9.9.9", &mut out, &mut err).unwrap();
            assert_eq!(outcome, RunOutcome::Exit(0));
            assert!(err.is_empty());
            assert!(String::from_utf8(out).unwrap().contains("9.9.9"));
        }
    }

    /// 不正なコマンドは `err` に出て非 0 終了。
    #[test]
    fn run_reports_unknown_command_on_stderr() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let outcome = run(
            argv(&["usagi", "nope-not-a-command"]),
            "9.9.9",
            &mut out,
            &mut err,
        )
        .unwrap();
        assert_eq!(outcome, RunOutcome::Exit(2));
        assert!(out.is_empty());
        assert!(!err.is_empty());
    }

    /// 余分な引数は clap の使い方エラーになり、TUI 起動要求へ到達しない。
    #[test]
    fn run_rejects_extra_tui_command_arguments_without_launching() {
        for tokens in [
            &["usagi", "hop", "extra"][..],
            &["usagi", "open", "one", "two"][..],
            &["usagi", "config", "extra"][..],
            &["usagi", "doctor", "extra"][..],
        ] {
            let mut out = Vec::new();
            let mut err = Vec::new();
            let outcome = run(argv(tokens), "9.9.9", &mut out, &mut err).unwrap();
            assert_eq!(outcome, RunOutcome::Exit(2));
            assert!(out.is_empty());
            assert!(!err.is_empty());
        }
    }

    /// clap が派生する update / 補助メタデータ関数も実行して被覆する
    /// （parse だけでは通らない `*_for_update` / `has_subcommand` 系を明示的に叩く）。
    #[test]
    fn exercises_clap_generated_metadata() {
        let _ = Cli::command_for_update();

        assert!(
            Command::augment_subcommands_for_update(clap::Command::new("usagi")).has_subcommands()
        );
        assert!(Command::has_subcommand("hop"));
        assert!(Command::has_subcommand("open"));
        assert!(!Command::has_subcommand("nope"));

        // FromArgMatches の update 経路。
        let matches = Cli::command()
            .try_get_matches_from(["usagi", "config"])
            .unwrap();
        let mut cli = Cli::from_arg_matches(&matches).unwrap();
        cli.update_from_arg_matches(&matches).unwrap();
        assert!(matches!(cli.command, Some(Command::Config)));

        // ValueEnum の派生メタデータ。
        assert_eq!(Shell::value_variants().len(), 5);
        assert!(Shell::Bash.to_possible_value().is_some());

        // CLI/TUI 境界型の derive された Clone / Debug / PartialEq を実行する。
        let request = TuiRequest::Workspace {
            path: Some("/tmp/x".into()),
        };
        assert_eq!(request.clone(), request);
        assert!(format!("{request:?}").contains("Workspace"));
        let outcome = RunOutcome::LaunchTui(TuiRequest::Doctor);
        assert_eq!(outcome.clone(), outcome);
        assert!(format!("{outcome:?}").contains("Doctor"));
    }
}

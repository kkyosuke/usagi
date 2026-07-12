//! 人間向け CLI サブコマンドの presentation。ここには **コマンド面の枠** だけを置く:
//! clap による引数解析ツリー（どんなコマンド・オプションがあるか）と、`Run`
//! トレイトによる多態 dispatch である。各コマンドの中身（core usecase 呼び出し・
//! daemon への IPC・結果整形）はハンドラ（`commands`）に実装していく。
//!
//! 現状ハンドラはすべて「未実装」を報告するスタブで、コマンド面の骨格だけが動く。
//! ロジックは usagi-core の usecase（store 系）と daemon への IPC（session 系）へ
//! 委譲する方針で、v2 では必要になった時点で各ハンドラを実装する。

pub mod commands;

use std::ffi::OsString;
use std::io::{self, Write};

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};

/// 実行可能な CLI サブコマンドの共通インターフェース。
///
/// clap が解釈した各コマンドは、自分の実行方法を知る型（`commands` のハンドラ）に
/// 変換され、dispatch は「型ごとに分岐する巨大な match」ではなく一様な `run` 呼び出しに
/// なる。出力先（`out`）は注入されるため、ハンドラは実 IO なしでユニットテストできる。
pub trait Run {
    /// サブコマンドを実行し、人間向けの出力を `out` に書き出す。
    ///
    /// # Errors
    ///
    /// `out` への書き込みに失敗した場合、そのエラーを返す。
    fn run(&self, out: &mut dyn Write) -> io::Result<()>;
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

/// 人間向け CLI サブコマンドの一覧。各バリアントがそのコマンドの受け付ける
/// オプションを型として表す。
#[derive(Debug, Subcommand)]
pub enum Command {
    /// カレントディレクトリをプロジェクトとして登録する（`--git` で clone してから登録）
    Init {
        /// このリポジトリ URL を `<リポジトリ名>/` に clone してから登録する
        #[arg(long, value_name = "URL")]
        git: Option<String>,
    },
    /// AI エージェント用の設定ファイル（CLAUDE.md など）を生成する
    InitAgent {
        /// 既存ファイルを確認なしで上書きする
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// カレントリポジトリの worktree 状態を state.json に同期して一覧表示する
    Status,
    /// デフォルトブランチを origin から最新化し各セッション worktree に配布する
    Update {
        /// 変更せず、更新・スキップ内容だけを表示する
        #[arg(long)]
        dry_run: bool,
    },
    /// 放置・マージ済みのセッション worktree を Agent CLI に整理させる
    Clean {
        /// 削除せず、削除候補と理由だけを報告させる
        #[arg(long)]
        dry_run: bool,
        /// この実行で使う Agent CLI を指定する（既定設定を上書き）
        #[arg(long, value_name = "NAME")]
        agent: Option<String>,
    },
    /// 必要ツールの導入状況を診断し、不足があれば導入を提案する
    Doctor {
        /// 確認を省いて不足を一括導入する
        #[arg(long)]
        fix: bool,
    },
    /// 各 Agent CLI が対応する usagi 機能を表で表示する
    Feature,
    /// 指定シェルの補完スクリプトを標準出力に印字する
    Completion {
        /// 補完スクリプトを生成する対象シェル
        shell: Shell,
    },
    /// workspace の `op://` 環境変数を解決するための 1Password 資格情報を保存する
    Op {
        #[command(subcommand)]
        command: OpCommand,
    },
    /// （ヘルプ非表示・上級者向け）グローバル設定を表示・編集する
    #[command(hide = true)]
    Config {
        /// 設定ファイルを $EDITOR で開いて編集し、保存時に検証する
        #[arg(long)]
        edit: bool,
    },
    /// （ヘルプ非表示・エージェント向け）.usagi/issues/ のタスク issue を操作する
    #[command(hide = true)]
    Issue {
        #[command(subcommand)]
        command: IssueCommand,
    },
    /// （ヘルプ非表示・エージェント向け）.usagi/memory/ のエージェントメモリを操作する
    #[command(hide = true)]
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
    },
}

/// `usagi op <subcommand>`。
#[derive(Debug, Subcommand)]
pub enum OpCommand {
    /// 1Password のサービスアカウントトークンを OS のキーチェーンに保存する
    Login,
}

/// `usagi issue <subcommand>`（エージェント向け。MCP と同じ store 操作の CLI 版）。
#[derive(Debug, Subcommand)]
pub enum IssueCommand {
    /// issue を新規作成する
    Create {
        /// issue のタイトル
        title: String,
    },
    /// issue を一覧表示する
    List,
    /// issue の依存グラフを表示する
    Graph,
    /// issue の詳細を表示する
    Show {
        /// issue 番号
        number: u32,
    },
    /// issue のメタデータ（status など）を更新する
    Update {
        /// issue 番号
        number: u32,
        /// 新しい status（todo / in-progress / done）
        #[arg(long)]
        status: Option<String>,
    },
    /// issue を全文検索する
    Search {
        /// 検索クエリ
        query: String,
    },
    /// issue を削除する
    Delete {
        /// issue 番号
        number: u32,
    },
}

/// `usagi memory <subcommand>`（エージェント向け。MCP と同じ store 操作の CLI 版）。
#[derive(Debug, Subcommand)]
pub enum MemoryCommand {
    /// メモリを保存する
    Save {
        /// メモリの名前（スラッグ）
        name: String,
    },
    /// メモリを一覧表示する
    List,
    /// メモリの詳細を表示する
    Show {
        /// メモリの名前
        name: String,
    },
    /// メモリを更新する
    Update {
        /// メモリの名前
        name: String,
    },
    /// メモリを全文検索する
    Search {
        /// 検索クエリ
        query: String,
    },
    /// メモリを削除する
    Delete {
        /// メモリの名前
        name: String,
    },
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
    /// 呼び出しになる。
    #[must_use]
    pub fn into_handler(self) -> Box<dyn Run> {
        use commands as h;
        match self {
            Command::Init { git } => Box::new(h::Init { git }),
            Command::InitAgent { yes } => Box::new(h::InitAgent { yes }),
            Command::Status => Box::new(h::Status),
            Command::Update { dry_run } => Box::new(h::Update { dry_run }),
            Command::Clean { dry_run, agent } => Box::new(h::Clean { dry_run, agent }),
            Command::Doctor { fix } => Box::new(h::Doctor { fix }),
            Command::Feature => Box::new(h::Feature),
            Command::Completion { shell } => Box::new(h::Completion { shell }),
            Command::Op { command } => Box::new(h::Op { command }),
            Command::Config { edit } => Box::new(h::Config { edit }),
            Command::Issue { command } => Box::new(h::Issue { command }),
            Command::Memory { command } => Box::new(h::Memory { command }),
        }
    }
}

/// CLI 面のエントリポイント。`args`（プログラム名を含む argv）を解析し、
/// 対応するハンドラを実行して、プロセスの終了コードを返す。
///
/// 出力は注入された `out` / `err` に書く（合成ルートが実 stdout / stderr を束ねる）。
/// clap の `--help` / `--version` は `out` に、使い方エラーは `err` に出す慣習に従う。
///
/// `args` は `OsString` の具体型で受ける（ジェネリックにすると呼び出し側の
/// イテレータ型ごとに単相化が増え、テストで到達しない実体がカバレッジを下げるため）。
/// `version` は `--version` に載せる配布 version（合成ルートが注入する）。
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
) -> io::Result<i32> {
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
            return Ok(e.exit_code());
        }
    };
    // matches は Cli::command() から得たものなので、Cli への変換は常に成功する。
    let cli =
        Cli::from_arg_matches(&matches).expect("matches from Cli::command() は Cli に変換できる");
    if let Some(command) = cli.command {
        command.into_handler().run(out)?;
        Ok(0)
    } else {
        // 引数なしの `usagi` は合成ルートが TUI に振り分けるため、ここに到達するのは
        // グローバルフラグだけが与えられた場合。トップレベルのヘルプを表示する。
        write!(out, "{}", command.render_long_help())?;
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command, IssueCommand, MemoryCommand, OpCommand, Shell, run};
    use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};

    /// `usagi status` を解析すると Status バリアントになる（parse 経路の疎通）。
    #[test]
    fn parses_a_simple_subcommand() {
        let cli = Cli::try_parse_from(["usagi", "status"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Status)));
    }

    /// オプションが解析されてバリアントのフィールドに載る。
    #[test]
    fn parses_options_onto_the_variant() {
        let cli = Cli::try_parse_from(["usagi", "update", "--dry-run"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Update { dry_run: true })
        ));

        let cli = Cli::try_parse_from(["usagi", "init", "--git", "https://x/y.git"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Init { git: Some(url) }) if url == "https://x/y.git"
        ));
    }

    /// ネストしたサブコマンドと `value_enum` も解析できる。
    #[test]
    fn parses_nested_subcommands_and_value_enum() {
        let cli = Cli::try_parse_from(["usagi", "op", "login"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Op {
                command: OpCommand::Login
            })
        ));

        let cli = Cli::try_parse_from(["usagi", "issue", "show", "42"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Issue {
                command: IssueCommand::Show { number: 42 }
            })
        ));

        let cli = Cli::try_parse_from(["usagi", "memory", "search", "worktree"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Memory {
                command: MemoryCommand::Search { .. }
            })
        ));

        let cli = Cli::try_parse_from(["usagi", "completion", "zsh"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Completion { shell: Shell::Zsh })
        ));
    }

    /// `&str` の並びを `run` が受け取る argv（`Vec<OsString>`）に変換する。
    fn argv(tokens: &[&str]) -> Vec<std::ffi::OsString> {
        tokens.iter().map(std::ffi::OsString::from).collect()
    }

    /// 有効なサブコマンドは終了コード 0 でハンドラ出力を `out` に書く。
    #[test]
    fn run_dispatches_to_a_handler() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(argv(&["usagi", "status"]), "9.9.9", &mut out, &mut err).unwrap();
        assert_eq!(code, 0);
        assert!(err.is_empty());
        assert!(String::from_utf8(out).unwrap().contains("status"));
    }

    /// サブコマンドなしはトップレベルのヘルプを `out` に出す。
    #[test]
    fn run_without_subcommand_prints_help() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(argv(&["usagi"]), "9.9.9", &mut out, &mut err).unwrap();
        assert_eq!(code, 0);
        assert!(String::from_utf8(out).unwrap().contains("Usage"));
    }

    /// `--help` は `out` に出て終了コード 0。
    #[test]
    fn run_help_goes_to_stdout() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(argv(&["usagi", "--help"]), "9.9.9", &mut out, &mut err).unwrap();
        assert_eq!(code, 0);
        assert!(err.is_empty());
        assert!(!out.is_empty());
    }

    /// `--version` は注入された配布 version を `out` に出す（cli クレートの 0.0.0 ではない）。
    #[test]
    fn run_version_uses_injected_value() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(argv(&["usagi", "--version"]), "9.9.9", &mut out, &mut err).unwrap();
        assert_eq!(code, 0);
        assert!(err.is_empty());
        assert!(String::from_utf8(out).unwrap().contains("9.9.9"));
    }

    /// 不正なコマンドは `err` に出て非 0 終了。
    #[test]
    fn run_reports_unknown_command_on_stderr() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(
            argv(&["usagi", "nope-not-a-command"]),
            "9.9.9",
            &mut out,
            &mut err,
        )
        .unwrap();
        assert_ne!(code, 0);
        assert!(out.is_empty());
        assert!(!err.is_empty());
    }

    /// clap が派生する update / 補助メタデータ関数も実行して被覆する
    /// （parse だけでは通らない `*_for_update` / `has_subcommand` 系を明示的に叩く）。
    #[test]
    fn exercises_clap_generated_metadata() {
        // CommandFactory の update 版。
        let _ = Cli::command_for_update();

        // 各 Subcommand enum の augment（update 版）と has_subcommand。
        assert!(
            Command::augment_subcommands_for_update(clap::Command::new("usagi")).has_subcommands()
        );
        assert!(Command::has_subcommand("status"));
        assert!(!Command::has_subcommand("nope"));
        assert!(
            OpCommand::augment_subcommands_for_update(clap::Command::new("op")).has_subcommands()
        );
        assert!(OpCommand::has_subcommand("login"));
        assert!(
            IssueCommand::augment_subcommands_for_update(clap::Command::new("issue"))
                .has_subcommands()
        );
        assert!(IssueCommand::has_subcommand("create"));
        assert!(
            MemoryCommand::augment_subcommands_for_update(clap::Command::new("memory"))
                .has_subcommands()
        );
        assert!(MemoryCommand::has_subcommand("save"));

        // FromArgMatches の update 経路。
        let matches = Cli::command()
            .try_get_matches_from(["usagi", "status"])
            .unwrap();
        let mut cli = Cli::from_arg_matches(&matches).unwrap();
        cli.update_from_arg_matches(&matches).unwrap();
        assert!(matches!(cli.command, Some(Command::Status)));

        // ValueEnum の派生メタデータ。
        assert_eq!(Shell::value_variants().len(), 5);
        assert!(Shell::Bash.to_possible_value().is_some());
    }
}

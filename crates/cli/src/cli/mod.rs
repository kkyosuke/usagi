//! 人間向け CLI サブコマンドの presentation。ここには **コマンド面の枠** だけを置く:
//! clap による引数解析ツリー（どんなコマンド・オプションがあるか）と、`Run`
//! トレイトによる多態 dispatch である。各コマンドの中身は今後ハンドラ（`commands`）に
//! 実装していく。
//!
//! ここに置くのは **ターミナルから `usagi <cmd>` で叩く人間向けコマンド** だけである。
//! エージェント向けの issue / memory 操作は MCP 面（`crate::mcp`）が受け持ち、CLI には置かない。
//!
//! 現状ハンドラはほぼ「未実装」を報告するスタブで、コマンド面の骨格だけが動く
//! （`version` だけは注入 version を表示する）。v2 では必要になった時点で各ハンドラを実装する。

pub mod commands;

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;

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
/// オプションを型として表す。`help` は clap が自動で用意する。
#[derive(Debug, Subcommand)]
pub enum Command {
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
            Command::Open { path } => Box::new(h::Open { path }),
            Command::Config => Box::new(h::Config),
            Command::Doctor => Box::new(h::Doctor),
            Command::Update => Box::new(h::Update),
            Command::Completion { shell } => Box::new(h::Completion { shell }),
            Command::Version => Box::new(h::Version {
                version: version.to_owned(),
            }),
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
        command.into_handler(version).run(out)?;
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
    use super::{Cli, Command, Shell, run};
    use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};

    /// `&str` の並びを `run` が受け取る argv（`Vec<OsString>`）に変換する。
    fn argv(tokens: &[&str]) -> Vec<std::ffi::OsString> {
        tokens.iter().map(std::ffi::OsString::from).collect()
    }

    /// オプションなしのサブコマンドを解析できる。
    #[test]
    fn parses_simple_subcommands() {
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

    /// 有効なサブコマンドは終了コード 0 でハンドラ出力を `out` に書く。
    #[test]
    fn run_dispatches_to_a_handler() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(argv(&["usagi", "config"]), "9.9.9", &mut out, &mut err).unwrap();
        assert_eq!(code, 0);
        assert!(err.is_empty());
        assert!(String::from_utf8(out).unwrap().contains("config"));
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

    /// `--version` フラグと `version` サブコマンドはどちらも注入された配布 version を出す。
    #[test]
    fn run_reports_injected_version() {
        for tokens in [&["usagi", "--version"][..], &["usagi", "version"][..]] {
            let mut out = Vec::new();
            let mut err = Vec::new();
            let code = run(argv(tokens), "9.9.9", &mut out, &mut err).unwrap();
            assert_eq!(code, 0);
            assert!(err.is_empty());
            assert!(String::from_utf8(out).unwrap().contains("9.9.9"));
        }
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
        let _ = Cli::command_for_update();

        assert!(
            Command::augment_subcommands_for_update(clap::Command::new("usagi")).has_subcommands()
        );
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
    }
}

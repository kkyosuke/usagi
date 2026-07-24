//! 人間向け CLI サブコマンドの presentation。ここには **コマンド面の枠** だけを置く:
//! clap による引数解析ツリー（どんなコマンド・オプションがあるか）と、`Run`
//! トレイトによる多態 dispatch である。各コマンドの中身は今後ハンドラ（`commands`）に
//! 実装していく。
//!
//! この tree は引数なし TUI、人間向け command、daemon control plane、MCP server を含む
//! process argv 全体を解釈する。エージェント向け issue / memory 操作そのものは MCP 面
//! （`crate::mcp`）が受け持ち、CLI command には置かない。
//!
//! TUI・daemon・MCP を開く command は typed な [`RunOutcome`] を返し、managed session
//! command は [`RunOutcome::DaemonRequest`] を返す。合成ルートは解析済み outcome だけを
//! 各実行面と終了 status へ接続する。

pub mod commands;
pub mod hooks;

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use usagi_core::usecase::claude_sandbox::SandboxMode;
use usagi_core::usecase::client::{DaemonRequest, SessionAction};

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
#[derive(Debug, Clone, PartialEq)]
pub enum RunOutcome {
    /// CLI で処理が完了したときのプロセス終了コード。
    Exit(i32),
    /// 合成ルートに TUI の起動を依頼する。
    LaunchTui(TuiRequest),
    /// daemon control plane の起動を依頼する。
    LaunchDaemon(DaemonCommand),
    /// Ask for an effect-free, coalesced build-artifact replacement trigger.
    /// The running daemon is not stopped by this ordinary client request.
    RequestDaemonReplacement,
    /// stdio MCP server の起動を依頼する。
    LaunchMcp,
    /// Codex `SessionStart` hook の structured payload を daemon へ渡す。
    CaptureCodexSession,
    /// Claude `PreToolUse` hook の payload を stdin から読み、worktree を出る
    /// ツール呼び出しなら deny 判定を stdout へ書く。判定は純粋（daemon 不要）。
    GuardWorkspace,
    /// OS sandbox の中で Claude を fail-closed 起動する。合成ルートが platform / backend /
    /// 環境を解決して sandbox を組み立て、backend 不在・未対応 platform では起動を拒否する。
    ClaudeSandbox {
        /// session（worktree 隔離）か root（コーディネータ）か。
        mode: SandboxMode,
        /// sandbox が書き込みを許す起動固有 root（複数指定可）。
        writable_roots: Vec<PathBuf>,
        /// sandbox の中で exec する program と引数（`claude …`）。
        command: Vec<String>,
    },
    /// A managed session mutation to be sent by the composition root through
    /// the daemon client. It deliberately is not executed against local state.
    DaemonRequest(DaemonRequest),
    /// Download and install the latest released binary through the composition root.
    SelfUpdate { command: String },
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
/// 引数なしの TUI、人間向け CLI、daemon control plane、MCP server を含む完全な argv を
/// 副作用より前にこの tree で解析する。合成ルートは解析済みの [`RunOutcome`] だけを実 IO
/// へ接続するため、特殊な実行面も未知 verb や余分な引数を黙殺しない。
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
/// 大半は人間向けだが、MCP と末尾の hook command は usagi が agent integration へ
/// 配線する内部入口で、`--help` には出さない（`hide = true`）。
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
    /// usagi バイナリを GitHub Releases から更新する
    Update {
        /// 更新先の release を一覧から選択する
        #[arg(short = 'v')]
        select_version: bool,
    },
    /// 指定シェルの補完スクリプトを標準出力に印字する（Tab 補完を有効化する）
    Completion {
        /// 補完スクリプトを生成する対象シェル
        shell: Shell,
    },
    /// バージョンを表示する
    Version,
    /// daemon process lifecycle を操作する
    Daemon {
        /// 実行する lifecycle verb。省略時は前景 serve。
        #[command(subcommand)]
        command: Option<DaemonCommand>,
    },
    /// （ヘルプ非表示・内部）stdio MCP server を起動する
    #[command(hide = true)]
    Mcp,
    /// Managed session lifecycle operation (always daemon-owned).
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// （ヘルプ非表示・内部）エージェントのライフサイクル phase を記録する（Stop フックが呼ぶ）
    #[command(hide = true)]
    AgentPhase {
        /// フックが報告する phase（例: `ended`）
        phase: String,
    },
    /// （ヘルプ非表示・内部）Codex `SessionStart` の session ID を daemon へ渡す。
    #[command(hide = true)]
    CodexSessionCapture,
    /// （ヘルプ非表示・内部）worktree の外へ出るツール呼び出しを拒否する（`PreToolUse` フックが呼ぶ）
    #[command(hide = true)]
    GuardWorkspace,
    /// （ヘルプ非表示・内部）OS sandbox の中で Claude を fail-closed 起動する
    #[command(hide = true)]
    ClaudeSandbox {
        /// 起動モード（session / root）
        #[arg(long)]
        mode: SandboxModeArg,
        /// sandbox が書き込みを許す起動固有 root（複数指定可）
        #[arg(long = "writable-root")]
        writable_root: Vec<PathBuf>,
        /// sandbox の中で exec する program と引数（`-- claude …`）
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
}

/// `usagi claude-sandbox --mode` が受け付ける起動モード。core の [`SandboxMode`] に写す
/// （core は clap に依存しないため、CLI 面がこの薄い写像を持つ）。`Debug` は `Command` の
/// derive、`Clone` は clap の値解析が使う。
#[derive(Debug, Clone, ValueEnum)]
pub enum SandboxModeArg {
    /// session worktree に隔離されたエージェント。
    Session,
    /// workspace root で動くコーディネータ。
    Root,
}

impl From<SandboxModeArg> for SandboxMode {
    fn from(mode: SandboxModeArg) -> Self {
        match mode {
            SandboxModeArg::Session => SandboxMode::Session,
            SandboxModeArg::Root => SandboxMode::Root,
        }
    }
}

/// daemon control plane が受理する閉じた lifecycle verb。
///
/// 引数なしの `usagi daemon` は [`DaemonCommand::Serve`] と同じである。各 variant は
/// 追加の positional/option を持たないため、clap が余分な argv を runtime 起動前に拒否する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
pub enum DaemonCommand {
    /// 前景で daemon を serve する（内部用）
    #[command(hide = true)]
    Serve,
    /// detached daemon を起動する
    Start,
    /// daemon の状態を表示する
    Status,
    /// daemon を停止する
    Stop,
    /// daemon を再起動する
    Restart,
    /// 現在 daemon の artifact を明示的に入れ替える trigger を要求する
    Replace,
    /// macOS `LaunchAgent` を install する
    InstallService,
    /// macOS `LaunchAgent` を uninstall する
    UninstallService,
}

/// The session mutations exposed by the human CLI.
#[derive(Debug, Subcommand)]
pub enum SessionCommand {
    Create {
        name: String,
    },
    Remove {
        name: String,
    },
    /// Explicitly resume the retained provider conversation in a new daemon
    /// Agent runtime. This command is never issued during startup/reconnect.
    Resume {
        name: String,
    },
    /// Resume one exact target returned by `resume-inventory`. The target is a
    /// secret-free JSON object; provider-native IDs are never accepted.
    ResumeExact {
        target: String,
    },
    /// List root and managed-session Agent resume targets for one workspace ID.
    ResumeInventory {
        workspace_id: String,
    },
    /// Validate legacy sessions without changing state unless `--apply` is set.
    RecoverLegacy {
        /// Persist the fully validated adoption plan.
        #[arg(long)]
        apply: bool,
    },
    Setup {
        name: String,
        command: String,
    },
    Prompt {
        name: String,
        prompt: String,
    },
}

impl Command {
    /// 解釈済みのコマンドを、その実行方法を知るハンドラ（`Run`）に変換する。
    ///
    /// dispatch はこの 1 か所の対応付けに集約し、実行自体は `Run::run` の一様な
    /// 呼び出しになる。`version` は `version` に合成ルートが注入する。
    #[must_use]
    #[coverage(off)]
    pub fn into_handler(self, version: &str) -> Box<dyn Run> {
        use commands as h;
        match self {
            Command::Hop => Box::new(h::Hop),
            Command::Open { path } => Box::new(h::Open { path }),
            Command::Config => Box::new(h::Config),
            Command::Doctor => Box::new(h::Doctor),
            Command::Update { select_version } => Box::new(h::Update { select_version }),
            Command::Completion { shell } => Box::new(h::Completion { shell }),
            Command::Version => Box::new(h::Version {
                version: version.to_owned(),
            }),
            Command::Daemon { command } => Box::new(DaemonEntry {
                command: command.unwrap_or(DaemonCommand::Serve),
            }),
            Command::Mcp => Box::new(McpEntry),
            Command::Session { command } => Box::new(Session { command }),
            // エージェント統合フックは commands/ ではなく hooks/ に置く。
            Command::AgentPhase { phase } => Box::new(hooks::AgentPhase { phase }),
            Command::CodexSessionCapture => Box::new(hooks::CodexSessionCapture),
            Command::GuardWorkspace => Box::new(hooks::GuardWorkspace),
            Command::ClaudeSandbox {
                mode,
                writable_root,
                command,
            } => Box::new(hooks::ClaudeSandbox {
                mode: mode.into(),
                writable_roots: writable_root,
                command,
            }),
        }
    }
}

struct DaemonEntry {
    command: DaemonCommand,
}

impl Run for DaemonEntry {
    fn run(&self, _out: &mut dyn Write) -> io::Result<RunOutcome> {
        Ok(if self.command == DaemonCommand::Replace {
            RunOutcome::RequestDaemonReplacement
        } else {
            RunOutcome::LaunchDaemon(self.command)
        })
    }
}

struct McpEntry;

impl Run for McpEntry {
    fn run(&self, _out: &mut dyn Write) -> io::Result<RunOutcome> {
        Ok(RunOutcome::LaunchMcp)
    }
}

struct Session {
    command: SessionCommand,
}

impl Run for Session {
    #[coverage(off)]
    fn run(&self, _out: &mut dyn Write) -> io::Result<RunOutcome> {
        let (action, payload) = match &self.command {
            SessionCommand::Create { name } => {
                (SessionAction::Create, serde_json::json!({"name": name}))
            }
            SessionCommand::Remove { name } => {
                (SessionAction::Remove, serde_json::json!({"name": name}))
            }
            SessionCommand::Resume { name } => (
                SessionAction::ResumeAgent,
                serde_json::json!({"name": name}),
            ),
            SessionCommand::ResumeExact { target } => {
                let target = serde_json::from_str(target).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "resume target must be a valid exact-target JSON object",
                    )
                })?;
                return Ok(RunOutcome::DaemonRequest(DaemonRequest::ResumeAgent {
                    operation_id: usagi_core::domain::id::OperationId::new().as_str(),
                    target,
                }));
            }
            SessionCommand::ResumeInventory { workspace_id } => {
                let workspace =
                    usagi_core::domain::id::WorkspaceId::parse(workspace_id).map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "workspace_id must be a canonical resource ID",
                        )
                    })?;
                return Ok(RunOutcome::DaemonRequest(DaemonRequest::AgentInventory {
                    workspace,
                }));
            }
            SessionCommand::RecoverLegacy { apply } => (
                SessionAction::RecoverLegacy,
                serde_json::json!({"apply": apply}),
            ),
            SessionCommand::Setup { name, command } => (
                SessionAction::Setup,
                serde_json::json!({"name": name, "command": command}),
            ),
            SessionCommand::Prompt { name, prompt } => (
                SessionAction::Prompt,
                serde_json::json!({"name": name, "prompt": prompt}),
            ),
        };
        Ok(RunOutcome::DaemonRequest(DaemonRequest::Session {
            action,
            operation_id: usagi_core::domain::id::OperationId::new().as_str(),
            payload,
        }))
    }
}

/// コマンドを dispatch してハンドラを実行し、結果と出力文字列を得るテストヘルパ。
/// `commands` / `hooks` 双方のハンドラテストから使い、`Command::into_handler` の各アームを被覆する。
#[cfg(test)]
#[coverage(off)]
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
#[coverage(off)]
pub fn run(
    args: Vec<OsString>,
    version: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> io::Result<RunOutcome> {
    let command = Cli::command().version(version.to_owned());
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
        Ok(RunOutcome::LaunchTui(TuiRequest::Welcome))
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command, DaemonCommand, RunOutcome, SessionCommand, Shell, TuiRequest, run};
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
            Some(Command::Update {
                select_version: false
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["usagi", "update", "-v"])
                .unwrap()
                .command,
            Some(Command::Update {
                select_version: true
            })
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
        assert!(matches!(
            Cli::try_parse_from(["usagi", "codex-session-capture"])
                .unwrap()
                .command,
            Some(Command::CodexSessionCapture)
        ));
    }

    /// daemon/MCP も通常 command と同じ clap tree を通り、typed な起動要求になる。
    #[test]
    fn special_entries_return_typed_launch_requests() {
        for (tokens, expected) in [
            (&["usagi", "daemon"][..], DaemonCommand::Serve),
            (&["usagi", "daemon", "serve"][..], DaemonCommand::Serve),
            (&["usagi", "daemon", "start"][..], DaemonCommand::Start),
            (&["usagi", "daemon", "status"][..], DaemonCommand::Status),
            (&["usagi", "daemon", "stop"][..], DaemonCommand::Stop),
            (&["usagi", "daemon", "restart"][..], DaemonCommand::Restart),
            (
                &["usagi", "daemon", "install-service"][..],
                DaemonCommand::InstallService,
            ),
            (
                &["usagi", "daemon", "uninstall-service"][..],
                DaemonCommand::UninstallService,
            ),
        ] {
            let mut out = Vec::new();
            let mut err = Vec::new();
            let outcome = run(argv(tokens), "9.9.9", &mut out, &mut err).unwrap();
            assert_eq!(outcome, RunOutcome::LaunchDaemon(expected));
            assert!(out.is_empty());
            assert!(err.is_empty());
        }

        let mut out = Vec::new();
        let mut err = Vec::new();
        let outcome = run(
            argv(&["usagi", "daemon", "replace"]),
            "9.9.9",
            &mut out,
            &mut err,
        )
        .unwrap();
        assert_eq!(outcome, RunOutcome::RequestDaemonReplacement);
        assert!(out.is_empty());
        assert!(err.is_empty());

        let mut out = Vec::new();
        let mut err = Vec::new();
        let outcome = run(argv(&["usagi", "mcp"]), "9.9.9", &mut out, &mut err).unwrap();
        assert_eq!(outcome, RunOutcome::LaunchMcp);
        assert!(out.is_empty());
        assert!(err.is_empty());
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

    #[test]
    fn session_commands_become_daemon_requests() {
        for (argv, action) in [
            (
                ["usagi", "session", "create", "a"].as_slice(),
                usagi_core::usecase::client::SessionAction::Create,
            ),
            (
                ["usagi", "session", "remove", "a"].as_slice(),
                usagi_core::usecase::client::SessionAction::Remove,
            ),
            (
                ["usagi", "session", "resume", "a"].as_slice(),
                usagi_core::usecase::client::SessionAction::ResumeAgent,
            ),
            (
                ["usagi", "session", "recover-legacy", "--apply"].as_slice(),
                usagi_core::usecase::client::SessionAction::RecoverLegacy,
            ),
            (
                ["usagi", "session", "setup", "a", "echo ok"].as_slice(),
                usagi_core::usecase::client::SessionAction::Setup,
            ),
            (
                ["usagi", "session", "prompt", "a", "hi"].as_slice(),
                usagi_core::usecase::client::SessionAction::Prompt,
            ),
        ] {
            let parsed = Cli::try_parse_from(argv).unwrap().command.unwrap();
            let (request, _) = super::execute(parsed);
            assert!(
                matches!(request, RunOutcome::DaemonRequest(usagi_core::usecase::client::DaemonRequest::Session { action: actual, .. }) if actual == action)
            );
        }
        assert!(matches!(
            SessionCommand::Create { name: "a".into() },
            SessionCommand::Create { .. }
        ));

        let target = usagi_core::domain::agent::AgentResumeTarget {
            continuation: usagi_core::domain::id::AgentContinuationRef::new(),
            source: usagi_core::domain::id::AgentResumeSourceId::new(),
            workspace_id: usagi_core::domain::id::WorkspaceId::new(),
            session_id: None,
            worktree_id: usagi_core::domain::id::WorktreeId::new(),
            runtime_id: usagi_core::domain::id::AgentRuntimeId::new(),
            adapter_revision: 1,
        };
        let (exact, _) = super::execute(Command::Session {
            command: SessionCommand::ResumeExact {
                target: serde_json::to_string(&target).unwrap(),
            },
        });
        assert!(matches!(
            exact,
            RunOutcome::DaemonRequest(usagi_core::usecase::client::DaemonRequest::ResumeAgent { target: actual, .. })
                if actual == target
        ));
        let (inventory, _) = super::execute(Command::Session {
            command: SessionCommand::ResumeInventory {
                workspace_id: target.workspace_id.to_string(),
            },
        });
        assert!(matches!(
            inventory,
            RunOutcome::DaemonRequest(usagi_core::usecase::client::DaemonRequest::AgentInventory { workspace })
                if workspace == target.workspace_id
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

    /// サブコマンドなしは副作用を起こさず Welcome TUI の起動要求を返す。
    #[test]
    fn run_without_subcommand_returns_welcome_request() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let outcome = run(argv(&["usagi"]), "9.9.9", &mut out, &mut err).unwrap();
        assert_eq!(outcome, RunOutcome::LaunchTui(TuiRequest::Welcome));
        assert!(out.is_empty());
        assert!(err.is_empty());
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

    /// 余分な引数と未知 daemon verb は clap の使い方エラーになり、起動要求へ到達しない。
    #[test]
    fn run_rejects_invalid_arguments_without_launching() {
        for tokens in [
            &["usagi", "hop", "extra"][..],
            &["usagi", "open", "one", "two"][..],
            &["usagi", "config", "extra"][..],
            &["usagi", "doctor", "extra"][..],
            &["usagi", "daemon", "bogus"][..],
            &["usagi", "daemon", "status", "extra"][..],
            &["usagi", "mcp", "extra"][..],
        ] {
            let mut out = Vec::new();
            let mut err = Vec::new();
            let outcome = run(argv(tokens), "9.9.9", &mut out, &mut err).unwrap();
            assert_eq!(outcome, RunOutcome::Exit(2));
            assert!(out.is_empty());
            assert!(String::from_utf8(err).unwrap().contains("Usage:"));
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

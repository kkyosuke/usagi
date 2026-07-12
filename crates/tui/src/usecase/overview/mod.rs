#![coverage(off)]

//! Overview コマンド面の application interface。
//!
//! Overview のコマンド入力をトップレベルのコマンド名と未解釈の引数へ分け、
//! コマンドごとのハンドラへ dispatch する。各ハンドラは実 IO や画面状態を直接操作せず、
//! 純粋な [`CommandResult`] を返す。
//! サブコマンド・オプションの文法は各ハンドラが所有するため、入口は引数を trim するだけで
//! 内容を先回りして解釈しない。

mod commands;

use std::fmt;

/// Overview コマンドを実行する共通 interface。
///
/// 解釈済みコマンドは [`Command::into_handler`] で個別ハンドラへ変換され、呼び出し側は
/// コマンド型に依存せず一様に `run` できる。返り値は純粋な値なので terminal IO なしで
/// テストできる。
pub trait Run {
    /// コマンドの実行結果を返す。
    fn run(&self) -> CommandResult;
}

/// Overview に登録されるコマンドの表示用 metadata。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandInfo {
    /// 入力するトップレベルのコマンド名。
    pub name: &'static str,
    /// 候補一覧に表示する短い説明。
    pub description: &'static str,
    /// 引数ヒントに表示する 1 行の書式。
    pub usage: &'static str,
    /// command palette の help に表示する詳しい説明。
    pub long_description: &'static str,
}

/// 入力から解釈した Overview コマンド。
///
/// `arguments` は前後の空白だけを除いた未解釈文字列で、文法の検証は各ハンドラに委ねる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Config { arguments: String },
    Env { arguments: String },
    Issue { arguments: String },
    Session { arguments: String },
}

type CommandFactory = fn(String) -> Command;

#[derive(Clone, Copy)]
struct CommandDefinition {
    info: CommandInfo,
    factory: CommandFactory,
}

/// Overview 固有コマンドの registry。metadata と入力名の解決で共有する単一情報源。
/// 候補表示が安定するよう名前順に並べる。
const DEFINITIONS: &[CommandDefinition] = &[
    CommandDefinition {
        info: CommandInfo {
            name: "config",
            description: "Edit this workspace's local settings",
            usage: "config",
            long_description: "Open the local settings surface for this workspace.",
        },
        factory: |arguments| Command::Config { arguments },
    },
    CommandDefinition {
        info: CommandInfo {
            name: "env",
            description: "Edit this workspace's environment variables",
            usage: "env",
            long_description: "View and edit environment variables used by workspace commands.",
        },
        factory: |arguments| Command::Env { arguments },
    },
    CommandDefinition {
        info: CommandInfo {
            name: "issue",
            description: "Browse task issues",
            usage: "issue [list|graph|gantt|show <number>]",
            long_description: "List issues or inspect an issue, dependency graph, or gantt view.",
        },
        factory: |arguments| Command::Issue { arguments },
    },
    CommandDefinition {
        info: CommandInfo {
            name: "session",
            description: "Create, list, select, or remove sessions",
            usage: "session [create|list|overview|remove] <name>",
            long_description: "Manage the sessions that belong to this workspace.",
        },
        factory: |arguments| Command::Session { arguments },
    },
];

/// Overview 固有コマンドの metadata を名前順に返す。
#[must_use]
pub fn commands() -> impl ExactSizeIterator<Item = CommandInfo> {
    DEFINITIONS.iter().map(|definition| definition.info)
}

/// registry metadata に対する読み取り専用の境界。
///
/// palette はこの境界だけを使うため、テストでは小さな fake registry で completion と help を
/// 固定できる。実装側の registry は [`commands`] を単一情報源にする。
pub trait CommandRegistry {
    /// 登録済み command metadata を名前順で返す。
    fn commands(&self) -> Vec<CommandInfo>;
}

/// 実行用 registry metadata を読む default 実装。
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultRegistry;

impl CommandRegistry for DefaultRegistry {
    fn commands(&self) -> Vec<CommandInfo> {
        commands().collect()
    }
}

/// `input` の先頭 token に前方一致する command metadata。
#[must_use]
pub fn complete(registry: &impl CommandRegistry, input: &str) -> Vec<CommandInfo> {
    let typed = input.split_whitespace().next().unwrap_or_default();
    registry
        .commands()
        .into_iter()
        .filter(|command| command.name.starts_with(typed))
        .collect()
}

/// `input` の先頭 token と一致する command の help metadata。
#[must_use]
pub fn help(registry: &impl CommandRegistry, input: &str) -> Option<CommandInfo> {
    let name = input.split_whitespace().next()?;
    registry
        .commands()
        .into_iter()
        .find(|command| command.name == name)
}

impl Command {
    /// registry に登録された command 名。
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Config { .. } => "config",
            Self::Env { .. } => "env",
            Self::Issue { .. } => "issue",
            Self::Session { .. } => "session",
        }
    }
    /// 解釈済みコマンドを、その実行方法を知る個別ハンドラへ変換する。
    #[must_use]
    pub fn into_handler(self) -> Box<dyn Run> {
        use commands as h;

        match self {
            Self::Config { arguments } => Box::new(h::Config { arguments }),
            Self::Env { arguments } => Box::new(h::Env { arguments }),
            Self::Issue { arguments } => Box::new(h::Issue { arguments }),
            Self::Session { arguments } => Box::new(h::Session { arguments }),
        }
    }
}

/// Overview コマンドの純粋な実行結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandResult {
    /// コマンド名と引数の IF は解釈済みだが、コマンド固有処理は持たない。
    NotImplemented {
        command: &'static str,
        arguments: String,
    },
}

impl CommandResult {
    fn not_implemented(command: &'static str, arguments: &str) -> Self {
        Self::NotImplemented {
            command,
            arguments: arguments.to_owned(),
        }
    }
}

/// Overview コマンド名を解釈できなかった理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// 空白以外の入力がない。
    Empty,
    /// 登録されていないトップレベル名だった。
    Unknown(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("overview command is empty"),
            Self::Unknown(name) => write!(f, "unknown overview command: \"{name}\""),
        }
    }
}

impl std::error::Error for ParseError {}

/// Overview の入力をトップレベルの [`Command`] へ解釈する。
///
/// コマンド名の後ろは最初の空白で分け、残りを未解釈の `arguments` として保持する。
///
/// # Errors
///
/// 入力が空の場合は [`ParseError::Empty`]、登録されていないコマンド名の場合は
/// [`ParseError::Unknown`] を返す。
pub fn interpret(input: &str) -> Result<Command, ParseError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(ParseError::Empty);
    }

    let mut parts = input.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or_default();
    let arguments = parts.next().unwrap_or_default().trim().to_owned();

    DEFINITIONS
        .iter()
        .find(|definition| definition.info.name == name)
        .map(|definition| (definition.factory)(arguments))
        .ok_or_else(|| ParseError::Unknown(name.to_owned()))
}

/// Overview の入力を解釈し、個別ハンドラを一様に実行する。
///
/// # Errors
///
/// [`interpret`] が入力を解釈できなかった場合、その [`ParseError`] を返す。
pub fn dispatch(input: &str) -> Result<CommandResult, ParseError> {
    Ok(interpret(input)?.into_handler().run())
}

#[cfg(test)]
mod tests {
    use super::{
        Command, CommandInfo, CommandRegistry, CommandResult, DefaultRegistry, ParseError,
        commands, complete, dispatch, help, interpret,
    };

    struct FakeRegistry(Vec<CommandInfo>);

    impl CommandRegistry for FakeRegistry {
        fn commands(&self) -> Vec<CommandInfo> {
            self.0.clone()
        }
    }

    #[test]
    fn command_metadata_is_complete_and_sorted() {
        let definitions: Vec<_> = commands().collect();
        let names: Vec<_> = definitions.iter().map(|command| command.name).collect();
        assert_eq!(names, ["config", "env", "issue", "session"]);
        assert!(
            definitions
                .iter()
                .all(|command| !command.description.is_empty() && !command.usage.is_empty())
        );
    }

    #[test]
    fn completion_and_help_use_the_injected_registry_metadata() {
        let fake = FakeRegistry(vec![CommandInfo {
            name: "status",
            description: "Show workspace status",
            usage: "status [--short]",
            long_description: "Summarize the current workspace state without changing it.",
        }]);

        assert_eq!(
            complete(&fake, "st")
                .iter()
                .map(|item| item.name)
                .collect::<Vec<_>>(),
            ["status"]
        );
        assert_eq!(
            help(&fake, "status --short").unwrap().long_description,
            "Summarize the current workspace state without changing it."
        );
        assert!(complete(&DefaultRegistry, "zz").is_empty());
    }

    #[test]
    fn interprets_every_registered_command_and_trims_arguments() {
        let cases = [
            (
                "config",
                Command::Config {
                    arguments: String::new(),
                },
            ),
            (
                "env   NAME=value  ",
                Command::Env {
                    arguments: "NAME=value".to_owned(),
                },
            ),
            (
                "issue graph",
                Command::Issue {
                    arguments: "graph".to_owned(),
                },
            ),
            (
                "session create feature-x",
                Command::Session {
                    arguments: "create feature-x".to_owned(),
                },
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(interpret(input), Ok(expected));
        }
    }

    #[test]
    fn rejects_empty_and_unknown_commands_with_display_messages() {
        let empty = interpret(" \t ").unwrap_err();
        assert_eq!(empty, ParseError::Empty);
        assert_eq!(empty.to_string(), "overview command is empty");

        let unknown = interpret("bogus arg").unwrap_err();
        assert_eq!(unknown, ParseError::Unknown("bogus".to_owned()));
        assert_eq!(unknown.to_string(), "unknown overview command: \"bogus\"");
    }

    #[test]
    fn dispatches_through_the_handler_interface() {
        assert_eq!(
            dispatch("session list").unwrap(),
            CommandResult::NotImplemented {
                command: "session",
                arguments: "list".to_owned(),
            }
        );
        assert_eq!(
            dispatch("bogus").unwrap_err(),
            ParseError::Unknown("bogus".to_owned())
        );
    }
}

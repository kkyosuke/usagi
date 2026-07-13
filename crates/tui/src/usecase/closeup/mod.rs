//! Closeup コマンド面の application interface。
//!
//! Closeup のコマンド入力をトップレベルのコマンド名と未解釈の引数へ分け、
//! コマンドごとのハンドラへ dispatch する。各ハンドラは実 IO や画面状態を直接操作せず、
//! 純粋な [`CommandResult`] を返す。
//! サブコマンド・オプションの文法は各ハンドラが所有するため、入口は引数を trim するだけで
//! 内容を先回りして解釈しない。

mod commands;

use std::fmt;

/// Closeup コマンドを実行する共通 interface。
///
/// 解釈済みコマンドは [`Command::into_handler`] で個別ハンドラへ変換され、呼び出し側は
/// コマンド型に依存せず一様に `run` できる。返り値は純粋な値なので terminal IO なしで
/// テストできる。
pub trait Run {
    /// コマンドの実行結果を返す。
    fn run(&self) -> CommandResult;
}

/// Closeup に登録されるコマンドの表示用 metadata。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandInfo {
    /// 入力するトップレベルのコマンド名。
    pub name: &'static str,
    /// 候補一覧に表示する短い説明。
    pub description: &'static str,
    /// 引数ヒントに表示する 1 行の書式。
    pub usage: &'static str,
}

/// 入力から解釈した Closeup コマンド。
///
/// `arguments` は前後の空白だけを除いた未解釈文字列で、文法の検証は各ハンドラに委ねる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Agent { arguments: String },
    Chat { arguments: String },
    Close { arguments: String },
    Diff { arguments: String },
    Terminal { arguments: String },
}

type CommandFactory = fn(String) -> Command;

#[derive(Clone, Copy)]
struct CommandDefinition {
    info: CommandInfo,
    factory: CommandFactory,
}

/// Closeup 固有コマンドの registry。metadata と入力名の解決で共有する単一情報源。
/// 候補表示が安定するよう名前順に並べる。
const DEFINITIONS: &[CommandDefinition] = &[
    CommandDefinition {
        info: CommandInfo {
            name: "agent",
            description: "Open an agent in the selected session",
            usage: "agent [name]",
        },
        factory: |arguments| Command::Agent { arguments },
    },
    CommandDefinition {
        info: CommandInfo {
            name: "chat",
            description: "Open local LLM chat in the selected session",
            usage: "chat",
        },
        factory: |arguments| Command::Chat { arguments },
    },
    CommandDefinition {
        info: CommandInfo {
            name: "close",
            description: "Remove the selected session",
            usage: "close [--force]",
        },
        factory: |arguments| Command::Close { arguments },
    },
    CommandDefinition {
        info: CommandInfo {
            name: "diff",
            description: "Show the selected session's diff",
            usage: "diff",
        },
        factory: |arguments| Command::Diff { arguments },
    },
    CommandDefinition {
        info: CommandInfo {
            name: "terminal",
            description: "Open a terminal in the selected session",
            usage: "terminal [open|new]",
        },
        factory: |arguments| Command::Terminal { arguments },
    },
];

/// Closeup 固有コマンドの metadata を名前順に返す。
#[must_use]
#[coverage(off)]
pub fn commands() -> impl ExactSizeIterator<Item = CommandInfo> {
    DEFINITIONS.iter().map(|definition| definition.info)
}

impl Command {
    /// registry に登録された command 名。
    #[must_use]
    #[coverage(off)]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Agent { .. } => "agent",
            Self::Chat { .. } => "chat",
            Self::Close { .. } => "close",
            Self::Diff { .. } => "diff",
            Self::Terminal { .. } => "terminal",
        }
    }
    /// 解釈済みコマンドを、その実行方法を知る個別ハンドラへ変換する。
    #[must_use]
    #[coverage(off)]
    pub fn into_handler(self) -> Box<dyn Run> {
        use commands as h;

        match self {
            Self::Agent { arguments } => Box::new(h::Agent { arguments }),
            Self::Chat { arguments } => Box::new(h::Chat { arguments }),
            Self::Close { arguments } => Box::new(h::Close { arguments }),
            Self::Diff { arguments } => Box::new(h::Diff { arguments }),
            Self::Terminal { arguments } => Box::new(h::Terminal { arguments }),
        }
    }
}

/// Closeup コマンドの純粋な実行結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandResult {
    /// コマンド名と引数の IF は解釈済みだが、コマンド固有処理は持たない。
    NotImplemented {
        command: &'static str,
        arguments: String,
    },
}

impl CommandResult {
    #[coverage(off)]
    fn not_implemented(command: &'static str, arguments: &str) -> Self {
        Self::NotImplemented {
            command,
            arguments: arguments.to_owned(),
        }
    }
}

/// Closeup コマンド名を解釈できなかった理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// 空白以外の入力がない。
    Empty,
    /// 登録されていないトップレベル名だった。
    Unknown(String),
}

impl fmt::Display for ParseError {
    #[coverage(off)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("closeup command is empty"),
            Self::Unknown(name) => write!(f, "unknown closeup command: \"{name}\""),
        }
    }
}

impl std::error::Error for ParseError {}

/// Closeup の入力をトップレベルの [`Command`] へ解釈する。
///
/// コマンド名の後ろは最初の空白で分け、残りを未解釈の `arguments` として保持する。
///
/// # Errors
///
/// 入力が空の場合は [`ParseError::Empty`]、登録されていないコマンド名の場合は
/// [`ParseError::Unknown`] を返す。
#[coverage(off)]
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

/// Closeup の入力を解釈し、個別ハンドラを一様に実行する。
///
/// # Errors
///
/// [`interpret`] が入力を解釈できなかった場合、その [`ParseError`] を返す。
#[coverage(off)]
pub fn dispatch(input: &str) -> Result<CommandResult, ParseError> {
    Ok(interpret(input)?.into_handler().run())
}

#[cfg(test)]
mod tests {
    use super::{Command, CommandResult, ParseError, commands, dispatch, interpret};

    #[test]
    #[coverage(off)]
    fn command_metadata_is_complete_and_sorted() {
        let definitions: Vec<_> = commands().collect();
        let names: Vec<_> = definitions.iter().map(|command| command.name).collect();
        assert_eq!(names, ["agent", "chat", "close", "diff", "terminal"]);
        assert!(
            definitions
                .iter()
                .all(|command| !command.description.is_empty() && !command.usage.is_empty())
        );
    }

    #[test]
    #[coverage(off)]
    fn interprets_every_registered_command_and_trims_arguments() {
        let cases = [
            (
                "agent   codex  ",
                Command::Agent {
                    arguments: "codex".to_owned(),
                },
            ),
            (
                "chat",
                Command::Chat {
                    arguments: String::new(),
                },
            ),
            (
                "close --force",
                Command::Close {
                    arguments: "--force".to_owned(),
                },
            ),
            (
                "diff",
                Command::Diff {
                    arguments: String::new(),
                },
            ),
            (
                "terminal new",
                Command::Terminal {
                    arguments: "new".to_owned(),
                },
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(interpret(input), Ok(expected));
        }
    }

    #[test]
    #[coverage(off)]
    fn rejects_empty_and_unknown_commands_with_display_messages() {
        let empty = interpret(" \t ").unwrap_err();
        assert_eq!(empty, ParseError::Empty);
        assert_eq!(empty.to_string(), "closeup command is empty");

        let unknown = interpret("bogus arg").unwrap_err();
        assert_eq!(unknown, ParseError::Unknown("bogus".to_owned()));
        assert_eq!(unknown.to_string(), "unknown closeup command: \"bogus\"");
    }

    #[test]
    #[coverage(off)]
    fn dispatches_through_the_handler_interface() {
        assert_eq!(
            dispatch("agent codex").unwrap(),
            CommandResult::NotImplemented {
                command: "agent",
                arguments: "codex".to_owned(),
            }
        );
        assert_eq!(
            dispatch("bogus").unwrap_err(),
            ParseError::Unknown("bogus".to_owned())
        );
    }
}

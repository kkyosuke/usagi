//! The [`CommandRegistry`]: the set of commands available in command mode,
//! dispatched and completed by name. Built with [`CommandRegistry::with_builtins`];
//! new commands can be added with [`CommandRegistry::register`].

use super::builtins::{
    AgentCommand, ClearCommand, CloseCommand, ComingSoonCommand, ConfigCommand, HistoryCommand,
    IssueCommand, ManCommand, QuitCommand, SessionCommand, TerminalCommand,
};
use super::{
    Command, CommandContext, CommandHint, CommandInfo, CommandResult, CommandScope, Completion,
    Hint, LogLine, WorktreeRef,
};
use crate::domain::issue::Issue;

/// The set of commands available in command mode, dispatched and completed by
/// name. Built with [`CommandRegistry::with_builtins`]; new commands can be
/// added with [`CommandRegistry::register`].
pub struct CommandRegistry {
    commands: Vec<Box<dyn Command>>,
}

impl CommandRegistry {
    /// A registry with every built-in command, in display order. The not-yet
    /// implemented feature commands (`ai`, `doctor`) are present as discoverable
    /// "coming soon" placeholders; `session` and `terminal` are fully implemented.
    pub fn with_builtins() -> Self {
        Self {
            commands: vec![
                Box::new(SessionCommand),
                Box::new(TerminalCommand),
                Box::new(AgentCommand),
                // The not-yet-implemented `ai` placeholder sits after the working
                // session commands so the 在席 (Focus) menu lists (and highlights)
                // `terminal` first, matching `document/design/05-home.md`.
                Box::new(ComingSoonCommand {
                    name: "ai",
                    description: "Talk to the AI agent",
                    usage: "ai <prompt>",
                    examples: &["ai fix the failing test"],
                    scope: CommandScope::Session,
                }),
                // `close` is the destructive session action, listed last in the
                // 在席 menu so it sits below the launch commands.
                Box::new(CloseCommand),
                Box::new(ConfigCommand),
                Box::new(IssueCommand),
                Box::new(HistoryCommand),
                Box::new(ComingSoonCommand {
                    name: "doctor",
                    description: "Check that required tools are installed",
                    usage: "doctor",
                    examples: &[],
                    scope: CommandScope::Workspace,
                }),
                Box::new(ManCommand),
                Box::new(ClearCommand),
                Box::new(QuitCommand),
            ],
        }
    }

    /// Add a command to the registry (used by follow-up command features).
    pub fn register(&mut self, command: Box<dyn Command>) {
        self.commands.push(command);
    }

    /// Name, description, usage, and examples of every command, in display order.
    pub(super) fn infos(&self) -> Vec<CommandInfo> {
        self.commands
            .iter()
            .map(|c| CommandInfo {
                name: c.name(),
                description: c.description(),
                usage: c.usage(),
                examples: c.examples(),
                scope: c.scope(),
            })
            .collect()
    }

    /// The commands belonging exactly to `scope`, in display order — used by the
    /// 在席 (Focus) menu to list a session's runnable commands (`terminal`,
    /// `agent`, `ai`). Unlike completion this is an exact-scope filter, so it
    /// excludes the shared [`CommandScope::Both`] utilities.
    pub fn commands_in_scope(&self, scope: CommandScope) -> Vec<CommandInfo> {
        self.infos()
            .into_iter()
            .filter(|i| i.scope == scope)
            .collect()
    }

    /// Find the command invoked by `name`, matching its primary name or any
    /// alias.
    fn find(&self, name: &str) -> Option<&dyn Command> {
        self.commands
            .iter()
            .find(|c| c.name() == name || c.aliases().contains(&name))
            .map(|c| c.as_ref())
    }

    /// Parse and run `input`, given the command `history` entered so far (not
    /// including the current input) and the workspace's `worktrees`. Returns the
    /// lines to append and a side effect. Unknown commands produce an error line.
    pub fn dispatch(
        &self,
        input: &str,
        history: &[String],
        worktrees: &[WorktreeRef],
    ) -> CommandResult {
        self.dispatch_with(input, history, worktrees, &[])
    }

    /// Like [`dispatch`](Self::dispatch) but also supplies the workspace's task
    /// `issues`, which the `issue` command reads. Kept separate so the many
    /// issue-agnostic call sites can stay on the shorter `dispatch`.
    pub fn dispatch_with(
        &self,
        input: &str,
        history: &[String],
        worktrees: &[WorktreeRef],
        issues: &[Issue],
    ) -> CommandResult {
        let trimmed = input.trim();
        let mut parts = trimmed.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("").trim();

        if name.is_empty() {
            return CommandResult::lines(Vec::new());
        }

        let infos = self.infos();
        let ctx = CommandContext {
            history,
            commands: &infos,
            worktrees,
            issues,
        };

        match self.find(name) {
            Some(command) => command.run(rest, &ctx),
            None => CommandResult::line(LogLine::error(format!(
                "unknown command: \"{name}\" (try \"man\")"
            ))),
        }
    }

    /// Complete the command word in `input` against the registered command
    /// names (aliases are not offered).
    ///
    /// Completion only applies to the first word: once the input contains
    /// whitespace (i.e. arguments are being typed) the input is returned
    /// unchanged. A unique match is filled in; an ambiguous one extends to the
    /// longest common prefix and reports the candidates; no match leaves the
    /// input untouched.
    pub fn complete(&self, input: &str, scope: CommandScope) -> Completion {
        if input.contains(char::is_whitespace) {
            return Completion {
                input: input.to_string(),
                candidates: Vec::new(),
            };
        }

        let matches: Vec<&str> = self
            .commands
            .iter()
            .filter(|c| c.scope().visible_in(scope))
            .map(|c| c.name())
            .filter(|name| name.starts_with(input))
            .collect();

        match matches.as_slice() {
            [] => Completion {
                input: input.to_string(),
                candidates: Vec::new(),
            },
            [only] => Completion {
                input: only.to_string(),
                candidates: Vec::new(),
            },
            many => Completion {
                input: common_prefix(many),
                candidates: many.iter().map(|name| name.to_string()).collect(),
            },
        }
    }

    /// Compute the advisory hint shown above the input as the user types.
    ///
    /// While the command word is being typed (no whitespace yet), it returns the
    /// commands whose name starts with what has been typed — every command when
    /// the input is empty, so the whole surface is discoverable the moment `:`
    /// is pressed. Once arguments are being given to a known command, it returns
    /// that command's usage syntax and examples instead. Unknown command words
    /// produce no hint. This is purely advisory; it never affects [`dispatch`].
    pub fn suggest(&self, input: &str, scope: CommandScope) -> Hint {
        let trimmed = input.trim_start();
        match trimmed.split_once(char::is_whitespace) {
            // Arguments are being typed: describe the resolved command, if known.
            Some((word, _)) => match self.find(word) {
                Some(command) => Hint::Usage {
                    usage: command.usage(),
                    examples: command.examples(),
                },
                None => Hint::None,
            },
            // Still on the command word: list the in-scope commands matching its
            // prefix (out-of-scope commands stay hidden so each mode's surface
            // is small and clear).
            None => {
                let hints: Vec<CommandHint> = self
                    .commands
                    .iter()
                    .filter(|c| c.scope().visible_in(scope))
                    .filter(|c| c.name().starts_with(trimmed))
                    .map(|c| CommandHint {
                        name: c.name(),
                        description: c.description(),
                    })
                    .collect();
                if hints.is_empty() {
                    Hint::None
                } else {
                    Hint::Commands(hints)
                }
            }
        }
    }
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

/// Longest common prefix shared by every string in `names`.
pub(super) fn common_prefix(names: &[&str]) -> String {
    let first = match names.first() {
        Some(first) => *first,
        None => return String::new(),
    };
    let mut end = first.len();
    for name in &names[1..] {
        end = end.min(name.len());
        while !name.is_char_boundary(end) || first[..end] != name[..end] {
            end -= 1;
        }
    }
    first[..end].to_string()
}

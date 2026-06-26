//! The [`CommandRegistry`]: the set of commands available in command mode,
//! dispatched and completed by name. Built with [`CommandRegistry::with_builtins`];
//! new commands can be added with [`CommandRegistry::register`].

use super::builtins::{
    AgentCommand, ClearCommand, CloseCommand, ComingSoonCommand, ConfigCommand, HistoryCommand,
    IssueCommand, ManCommand, PreviewCommand, QuitCommand, SessionCommand, TerminalCommand,
};
use super::{
    Command, CommandContext, CommandHint, CommandInfo, CommandResult, CommandScope, Completion,
    CompletionContext, Hint, LogLine, WorktreeRef,
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
                Box::new(PreviewCommand),
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

    /// Complete the `input` in `scope`, matching the spelling under the caret.
    ///
    /// While the command word is being typed (no whitespace yet) it completes
    /// against the in-scope command names (aliases are not offered). Once the
    /// command word is settled and arguments are being typed, it delegates to
    /// the resolved command's [`Command::complete_args`] to complete the current
    /// option/subcommand token, leaving the command word and earlier arguments
    /// untouched. Either way a unique match is filled in; an ambiguous one
    /// extends to the longest common prefix and reports the candidates; no match
    /// leaves the input untouched.
    pub fn complete(&self, input: &str, scope: CommandScope) -> Completion {
        self.complete_with(input, scope, &[])
    }

    /// Like [`complete`](Self::complete) but also supplies the workspace's
    /// `session_names`, which `session switch`/`remove` complete their `<name>`
    /// argument against. Kept separate so the many session-agnostic call sites
    /// (and `man`'s own argument completion) can stay on the shorter `complete`.
    pub fn complete_with(
        &self,
        input: &str,
        scope: CommandScope,
        session_names: &[&str],
    ) -> Completion {
        let Some((word, args)) = input.split_once(char::is_whitespace) else {
            // Still on the command word: complete against the in-scope names.
            let matches: Vec<&str> = self
                .commands
                .iter()
                .filter(|c| c.scope().visible_in(scope))
                .map(|c| c.name())
                .filter(|name| name.starts_with(input))
                .collect();
            // The whole input is the token being completed (nothing precedes it).
            return complete_token(input, input, &matches);
        };

        // Arguments are being typed: complete the current token against the
        // resolved command's argument vocabulary. An unknown or out-of-scope
        // command offers nothing, so the input is returned unchanged.
        let Some(command) = self.find(word).filter(|c| c.scope().visible_in(scope)) else {
            return Completion {
                input: input.to_string(),
                candidates: Vec::new(),
            };
        };
        let command_names: Vec<&str> = self.commands.iter().map(|c| c.name()).collect();
        let ctx = CompletionContext {
            command_names: &command_names,
            session_names,
        };
        let candidates = command.complete_args(args, &ctx);
        let (_, partial) = arg_tokens(args);
        let matches: Vec<&str> = candidates
            .iter()
            .map(String::as_str)
            .filter(|c| c.starts_with(partial))
            .collect();
        complete_token(input, partial, &matches)
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

/// Split a command's argument string into its already-complete tokens and the
/// final, still-being-typed token. A trailing space means the user has finished
/// the previous token and is starting a new one, so the partial token is empty
/// and every typed token is complete; otherwise the last whitespace-separated
/// token is the partial one. Used both to decide which option position is being
/// completed (the complete tokens) and what its prefix is (the partial token).
pub(super) fn arg_tokens(args: &str) -> (Vec<&str>, &str) {
    if args.is_empty() || args.ends_with(char::is_whitespace) {
        (args.split_whitespace().collect(), "")
    } else {
        let mut tokens: Vec<&str> = args.split_whitespace().collect();
        let partial = tokens.pop().unwrap_or("");
        (tokens, partial)
    }
}

/// Complete the final `partial` token of `input` against the already
/// prefix-matched `matches`, rewriting only that token: a unique match is filled
/// in, an ambiguous one extends to the longest common prefix and reports the
/// candidates, and no match leaves the input untouched. Everything before the
/// partial token (the command word and earlier arguments, with their spacing) is
/// preserved. `partial` must be a suffix of `input` (it is, being its final
/// token), so trimming it off lands on a char boundary.
fn complete_token(input: &str, partial: &str, matches: &[&str]) -> Completion {
    let fixed = &input[..input.len() - partial.len()];
    match matches {
        [] => Completion {
            input: input.to_string(),
            candidates: Vec::new(),
        },
        [only] => Completion {
            input: format!("{fixed}{only}"),
            candidates: Vec::new(),
        },
        many => Completion {
            input: format!("{fixed}{}", common_prefix(many)),
            candidates: many.iter().map(|name| name.to_string()).collect(),
        },
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

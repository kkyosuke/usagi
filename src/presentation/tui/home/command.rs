//! The workspace screen's command mode: a small, extensible command shell.
//!
//! In command mode the user types a command, which is turned into log lines and
//! a side effect. Commands are not hard-coded into a `match`; each is a value
//! implementing the [`Command`] trait, collected in a [`CommandRegistry`]. This
//! is the extension point the follow-up command issues (`session`, `space`,
//! `ai`, `terminal`, …) plug into: implement [`Command`] and register it.
//!
//! Everything here is pure (no terminal IO), so the whole command surface —
//! dispatch, completion, and each command's behaviour — is directly testable.

use super::state::LogLine;

/// A side effect a command asks the screen / event loop to perform, beyond
/// appending its produced log lines.
#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    /// Nothing extra — just append the produced lines.
    None,
    /// Clear the output log.
    Clear,
    /// Quit the whole application.
    Quit,
    /// Open the session-name modal (the user ran `session create` without a
    /// name).
    OpenSessionModal,
    /// Create a session with the given name (the user supplied one).
    CreateSession(String),
    /// List the workspace's sessions (the user ran `session list`).
    ListSessions,
    /// Remove a session (the user ran `session remove <name> [--force]`).
    RemoveSession { name: String, force: bool },
    /// Open the session-removal modal (the user ran `session remove` without a
    /// name) to pick one or more sessions to delete at once. `force` carries the
    /// `--force` flag so the confirmed removal can discard uncommitted changes.
    OpenRemoveModal { force: bool },
    /// Enter 切替 (Switch) to pick a session in the left pane (the user ran
    /// `session switch` with no name).
    EnterSwitch,
    /// Focus the session named by the string (the user ran `session switch
    /// <name>`). The event loop resolves the name against the worktree list and,
    /// for a live session, attaches the pane.
    Activate(String),
    /// Open an interactive terminal in the selected worktree (the user ran
    /// `terminal`). The directory is resolved by the event loop.
    OpenTerminal,
    /// Open the configured AI agent in the selected worktree (the user ran
    /// `agent`). This is `terminal` with the agent CLI launched inside it; the
    /// directory and agent command are resolved by the event loop / wiring.
    OpenAgent,
    /// Open the configuration screen (the user ran `config`) to edit the global
    /// settings and this workspace's local overrides. The screen is run by the
    /// event loop, which returns to the workspace screen when it is dismissed.
    OpenConfig,
}

/// The result of running a command: lines to append plus a side effect.
#[derive(Debug)]
pub struct CommandResult {
    pub lines: Vec<LogLine>,
    pub effect: Effect,
}

impl CommandResult {
    /// A result that only appends `lines`, with no extra side effect.
    fn lines(lines: Vec<LogLine>) -> Self {
        Self {
            lines,
            effect: Effect::None,
        }
    }

    /// A result that appends a single line, with no extra side effect.
    fn line(line: LogLine) -> Self {
        Self::lines(vec![line])
    }
}

/// Which of the home screen's command scopes a command belongs to.
///
/// The two surfaces are *physically separate* in the redesigned home screen
/// (統括 / 切替 / 在席 / 没入, see `document/design/05-home.md`): the bottom
/// command line in *統括 (Overview)* operates the whole workspace
/// ([`CommandScope::Workspace`]), while the *在席 (Focus)* right pane operates one
/// session ([`CommandScope::Session`]). Because the two never share a line, the
/// scopes do not nest — a command is offered only in its own scope (plus the
/// shared utilities). Commands are offered (completion, hints, `man` grouping)
/// accordingly; [`CommandScope::Both`] commands are utilities available
/// everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandScope {
    /// Operating the whole workspace, the *統括 (Overview)* line: `session`,
    /// `config`, `doctor`.
    Workspace,
    /// Operating a single session, the *在席 (Focus)* right pane: `terminal`,
    /// `agent`, `ai`.
    Session,
    /// A utility available in every scope: `man`, `history`, `clear`, `quit`.
    Both,
}

impl CommandScope {
    /// Whether a command of this scope is offered while the screen is in
    /// `current` scope. A command is offered in its own scope only;
    /// [`CommandScope::Both`] utilities are offered everywhere.
    pub fn visible_in(self, current: CommandScope) -> bool {
        self == CommandScope::Both || self == current
    }
}

/// Name, description, and usage detail of a registered command, exposed to
/// commands (via [`CommandContext`]) so e.g. `man` can list the whole surface,
/// and describe any single command, without reaching back into the registry.
#[derive(Debug, Clone, Copy)]
pub struct CommandInfo {
    pub name: &'static str,
    pub description: &'static str,
    /// One-line usage syntax, e.g. `man [command]`.
    pub usage: &'static str,
    /// Example invocations shown by `man <command>`.
    pub examples: &'static [&'static str],
    /// Which command scope it belongs to, for `man`'s grouping.
    pub scope: CommandScope,
}

/// A worktree as seen by commands: its display name and whether it is the
/// currently active one. Exposed via [`CommandContext`] so `space` can list the
/// available worktrees without reaching into the screen state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRef {
    pub name: String,
    pub active: bool,
}

/// Everything a command may read while running, beyond its own argument string.
pub struct CommandContext<'a> {
    /// Commands entered so far this session (oldest first), for `history`.
    pub history: &'a [String],
    /// Every registered command, in display order, for `man`.
    pub commands: &'a [CommandInfo],
    /// The workspace's worktrees, in display order, for `space`.
    pub worktrees: &'a [WorktreeRef],
}

/// A command available in the workspace screen's command mode.
///
/// Implementors are registered in a [`CommandRegistry`]. The trait is
/// object-safe so heterogeneous commands can live together in the registry.
pub trait Command {
    /// The command word the user types (e.g. `"man"`).
    fn name(&self) -> &'static str;

    /// A one-line description, shown by `man`.
    fn description(&self) -> &'static str;

    /// Extra names that also invoke this command (e.g. `"help"` for `man`).
    /// Aliases are dispatchable but are not offered as completions.
    fn aliases(&self) -> &'static [&'static str] {
        &[]
    }

    /// One-line usage syntax shown by `man <command>`. Defaults to just the
    /// command name (i.e. the command takes no arguments).
    fn usage(&self) -> &'static str {
        self.name()
    }

    /// Example invocations shown by `man <command>`. Empty by default.
    fn examples(&self) -> &'static [&'static str] {
        &[]
    }

    /// Which command scope the command belongs to. Defaults to
    /// [`CommandScope::Both`] (a utility offered in every scope); the
    /// workspace- and session-specific commands override it.
    fn scope(&self) -> CommandScope {
        CommandScope::Both
    }

    /// Run the command with its (trimmed) argument string and the context.
    fn run(&self, args: &str, ctx: &CommandContext) -> CommandResult;
}

/// `man` / `help`: lists every command, or describes one.
struct ManCommand;

impl Command for ManCommand {
    fn name(&self) -> &'static str {
        "man"
    }

    fn description(&self) -> &'static str {
        "Show help for commands (man <command> for details)"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["help"]
    }

    fn usage(&self) -> &'static str {
        "man [command]"
    }

    fn examples(&self) -> &'static [&'static str] {
        &["man", "man session"]
    }

    fn run(&self, args: &str, ctx: &CommandContext) -> CommandResult {
        if args.is_empty() {
            let mut lines = vec![LogLine::output("Available commands:")];
            // Group by scope so the two modes are obvious: workspace-wide
            // commands first, then per-session ones, then the utilities.
            for (scope, header) in [
                (CommandScope::Workspace, "Workspace (root):"),
                (CommandScope::Session, "Session (selected):"),
                (CommandScope::Both, "General:"),
            ] {
                lines.push(LogLine::output(format!("  {header}")));
                for info in ctx.commands.iter().filter(|i| i.scope == scope) {
                    lines.push(LogLine::output(format!(
                        "    {:<9}{}",
                        info.name, info.description
                    )));
                }
            }
            lines.push(LogLine::output(
                "Type \"man <command>\" for usage and examples.",
            ));
            return CommandResult::lines(lines);
        }

        match ctx.commands.iter().find(|info| info.name == args) {
            Some(info) => CommandResult::lines(describe(info)),
            None => CommandResult::line(LogLine::error(format!("no manual entry for \"{args}\""))),
        }
    }
}

/// The detailed help shown by `man <command>`: a header, a usage line, and any
/// example invocations.
fn describe(info: &CommandInfo) -> Vec<LogLine> {
    let mut lines = vec![
        LogLine::output(format!("{} — {}", info.name, info.description)),
        LogLine::output("Usage:"),
        LogLine::output(format!("  {}", info.usage)),
    ];
    if !info.examples.is_empty() {
        lines.push(LogLine::output("Examples:"));
        for example in info.examples {
            lines.push(LogLine::output(format!("  {example}")));
        }
    }
    lines
}

/// `history`: lists the commands entered so far this session.
struct HistoryCommand;

impl Command for HistoryCommand {
    fn name(&self) -> &'static str {
        "history"
    }

    fn description(&self) -> &'static str {
        "Show the command history"
    }

    fn run(&self, _args: &str, ctx: &CommandContext) -> CommandResult {
        if ctx.history.is_empty() {
            return CommandResult::line(LogLine::output("No commands in history yet."));
        }
        let lines = ctx
            .history
            .iter()
            .enumerate()
            .map(|(i, entry)| LogLine::output(format!("  {:>3}  {entry}", i + 1)))
            .collect();
        CommandResult::lines(lines)
    }
}

/// `clear`: empties the output pane.
struct ClearCommand;

impl Command for ClearCommand {
    fn name(&self) -> &'static str {
        "clear"
    }

    fn description(&self) -> &'static str {
        "Clear the output pane"
    }

    fn run(&self, _args: &str, _ctx: &CommandContext) -> CommandResult {
        CommandResult {
            lines: Vec::new(),
            effect: Effect::Clear,
        }
    }
}

/// `quit` / `exit`: leaves usagi and returns to the project list.
struct QuitCommand;

impl Command for QuitCommand {
    fn name(&self) -> &'static str {
        "quit"
    }

    fn description(&self) -> &'static str {
        "Leave usagi and return to the project list"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["exit"]
    }

    fn run(&self, _args: &str, _ctx: &CommandContext) -> CommandResult {
        CommandResult {
            lines: vec![LogLine::output("USAGI run away ( ^-^)ノ")],
            effect: Effect::Quit,
        }
    }
}

/// `session`: create, list, or switch sessions (a branch + worktree per repo).
///
/// Each subcommand accepts short aliases: `create` (`c`, `new`), `list` (`ls`),
/// and `remove` (`rm`).
///
/// - `session create <name>` creates a session; `session create` with no name
///   returns [`Effect::OpenSessionModal`] so the screen can prompt.
/// - `session list` lists the sessions.
/// - `session switch <name>` switches the active session (via
///   [`Effect::Activate`]); `session switch` with no name lists the sessions and
///   marks the active one.
/// - `session remove <name> [--force]` removes a session; `session remove` with
///   no name returns [`Effect::OpenRemoveModal`] so the screen can show a
///   checklist of sessions to delete in one go.
struct SessionCommand;

impl Command for SessionCommand {
    fn name(&self) -> &'static str {
        "session"
    }

    fn description(&self) -> &'static str {
        "Create, list, or switch sessions (branch + worktree)"
    }

    fn usage(&self) -> &'static str {
        "session [create|list|switch|remove] <name>  (aliases: create=c/new, list=ls, remove=rm)"
    }

    fn examples(&self) -> &'static [&'static str] {
        &[
            "session create feature-x",
            "session switch feature-x",
            "session ls",
            "session rm feature-x",
        ]
    }

    fn scope(&self) -> CommandScope {
        CommandScope::Workspace
    }

    fn run(&self, args: &str, ctx: &CommandContext) -> CommandResult {
        let mut parts = args.splitn(2, char::is_whitespace);
        let sub = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("").trim();

        // Normalize subcommand aliases to their canonical name so the rest of the
        // dispatch only deals with one spelling each.
        let sub = match sub {
            "create" | "c" | "new" => "create",
            "list" | "ls" => "list",
            "remove" | "rm" => "remove",
            other => other,
        };

        let open = || CommandResult {
            lines: Vec::new(),
            effect: Effect::OpenSessionModal,
        };
        let create = |name: &str| CommandResult {
            lines: Vec::new(),
            effect: Effect::CreateSession(name.to_string()),
        };

        match sub {
            "create" if rest.is_empty() => open(),
            "create" => create(rest),
            "list" => CommandResult {
                lines: Vec::new(),
                effect: Effect::ListSessions,
            },
            "switch" => switch(rest, ctx),
            "remove" => {
                let mut force = false;
                let mut target = None;
                for tok in rest.split_whitespace() {
                    match tok {
                        "--force" | "-f" => force = true,
                        _ if target.is_none() => target = Some(tok.to_string()),
                        _ => {}
                    }
                }
                match target {
                    // A name removes that session directly.
                    Some(name) => CommandResult {
                        lines: Vec::new(),
                        effect: Effect::RemoveSession { name, force },
                    },
                    // No name: open the picker to remove one or more at once.
                    None => CommandResult {
                        lines: Vec::new(),
                        effect: Effect::OpenRemoveModal { force },
                    },
                }
            }
            _ => CommandResult::line(LogLine::error(format!("usage: {}", self.usage()))),
        }
    }
}

/// `session switch [name]`: enter 切替 (Switch) to pick a session in the left
/// pane when no name is given ([`Effect::EnterSwitch`]), or focus the named one
/// directly ([`Effect::Activate`]).
///
/// Either way the mode transition (and, for a live session, attaching the pane)
/// happens in the event loop, which owns the worktree list and the modes.
fn switch(name: &str, _ctx: &CommandContext) -> CommandResult {
    if name.is_empty() {
        return CommandResult {
            lines: Vec::new(),
            effect: Effect::EnterSwitch,
        };
    }

    CommandResult {
        lines: Vec::new(),
        effect: Effect::Activate(name.to_string()),
    }
}

/// `terminal`: open an interactive shell in the selected worktree, or at the
/// workspace root when the root row is selected. The spawn is a side effect
/// ([`Effect::OpenTerminal`]) performed by the event loop, which holds the
/// worktree paths and the terminal handle.
struct TerminalCommand;

impl Command for TerminalCommand {
    fn name(&self) -> &'static str {
        "terminal"
    }

    fn description(&self) -> &'static str {
        "Open an interactive terminal in the selected session"
    }

    fn scope(&self) -> CommandScope {
        CommandScope::Session
    }

    fn run(&self, _args: &str, _ctx: &CommandContext) -> CommandResult {
        CommandResult {
            lines: Vec::new(),
            effect: Effect::OpenTerminal,
        }
    }
}

/// `agent`: open the configured AI agent in the selected worktree. This is a
/// shortcut for running `terminal` and then launching the agent CLI inside it,
/// so it produces the same [`Effect::OpenAgent`] side effect the event loop
/// turns into an embedded terminal with the agent command sent on start.
struct AgentCommand;

impl Command for AgentCommand {
    fn name(&self) -> &'static str {
        "agent"
    }

    fn description(&self) -> &'static str {
        "Open the AI agent in the selected session (terminal + agent CLI)"
    }

    fn scope(&self) -> CommandScope {
        CommandScope::Session
    }

    fn run(&self, _args: &str, _ctx: &CommandContext) -> CommandResult {
        CommandResult {
            lines: Vec::new(),
            effect: Effect::OpenAgent,
        }
    }
}

/// `config`: open the configuration screen to edit **this workspace's** local
/// overrides (the global settings are edited from the CLI or welcome menu).
/// Opening the screen is a side effect ([`Effect::OpenConfig`]) performed by the
/// event loop, which owns the terminal and returns to the workspace screen when
/// the user dismisses it.
struct ConfigCommand;

impl Command for ConfigCommand {
    fn name(&self) -> &'static str {
        "config"
    }

    fn description(&self) -> &'static str {
        "Edit this workspace's local settings"
    }

    fn scope(&self) -> CommandScope {
        CommandScope::Workspace
    }

    fn run(&self, _args: &str, _ctx: &CommandContext) -> CommandResult {
        CommandResult {
            lines: Vec::new(),
            effect: Effect::OpenConfig,
        }
    }
}

/// A recognised command whose real behaviour is not built yet. It produces a
/// friendly "coming soon" line so the surface stays discoverable; the follow-up
/// issues replace each one with a real [`Command`] implementation. It still
/// carries usage/examples so `man <command>` is useful ahead of implementation.
struct ComingSoonCommand {
    name: &'static str,
    description: &'static str,
    usage: &'static str,
    examples: &'static [&'static str],
    scope: CommandScope,
}

impl Command for ComingSoonCommand {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn usage(&self) -> &'static str {
        self.usage
    }

    fn examples(&self) -> &'static [&'static str] {
        self.examples
    }

    fn scope(&self) -> CommandScope {
        self.scope
    }

    fn run(&self, _args: &str, _ctx: &CommandContext) -> CommandResult {
        CommandResult::line(LogLine::output(format!(
            "\"{}\" is coming soon 🐰",
            self.name
        )))
    }
}

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
                Box::new(ConfigCommand),
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
    fn infos(&self) -> Vec<CommandInfo> {
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

/// The result of a Tab completion: the (possibly extended) input, plus the
/// candidate command names when the completion is ambiguous.
#[derive(Debug, PartialEq, Eq)]
pub struct Completion {
    pub input: String,
    pub candidates: Vec<String>,
}

/// One command offered in the input hints: its name and one-line description.
#[derive(Debug, PartialEq, Eq)]
pub struct CommandHint {
    pub name: &'static str,
    pub description: &'static str,
}

/// The advisory hint rendered above the command input, computed by
/// [`CommandRegistry::suggest`] from the current input.
#[derive(Debug, PartialEq, Eq)]
pub enum Hint {
    /// The command word is being typed: the matching commands (every command
    /// when the input is empty), in display order.
    Commands(Vec<CommandHint>),
    /// A known command is being given arguments: its usage syntax and examples.
    Usage {
        usage: &'static str,
        examples: &'static [&'static str],
    },
    /// Nothing to suggest (e.g. an unrecognised command word).
    None,
}

/// Longest common prefix shared by every string in `names`.
fn common_prefix(names: &[&str]) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::tui::home::state::LineKind;

    fn registry() -> CommandRegistry {
        CommandRegistry::with_builtins()
    }

    #[test]
    fn empty_input_does_nothing() {
        let result = registry().dispatch("   ", &[], &[]);
        assert!(result.lines.is_empty());
        assert_eq!(result.effect, Effect::None);
    }

    #[test]
    fn man_without_argument_lists_every_command() {
        let registry = registry();
        let result = registry.dispatch("man", &[], &[]);
        assert_eq!(result.effect, Effect::None);
        let joined = result
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("Available commands"));
        for info in registry.infos() {
            assert!(joined.contains(info.name));
        }
    }

    #[test]
    fn help_is_an_alias_for_man() {
        let result = registry().dispatch("help", &[], &[]);
        assert!(result.lines[0].text.contains("Available commands"));
    }

    #[test]
    fn man_without_argument_hints_at_per_command_help() {
        let result = registry().dispatch("man", &[], &[]);
        let last = result.lines.last().unwrap();
        assert!(last.text.contains("man <command>"));
    }

    #[test]
    fn man_with_a_known_command_shows_usage_and_examples() {
        let result = registry().dispatch("man session", &[], &[]);
        assert!(result.lines.len() > 1);
        // Header, then a Usage block, then an Examples block.
        assert_eq!(result.lines[0].kind, LineKind::Output);
        assert!(result.lines[0].text.starts_with("session —"));
        let joined = result
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("Usage:"));
        assert!(joined.contains("session [create|list|switch|remove] <name>"));
        assert!(joined.contains("Examples:"));
        assert!(joined.contains("session switch feature-x"));
    }

    #[test]
    fn man_with_a_command_without_examples_omits_the_examples_block() {
        // `clear` takes no arguments and has no examples (trait defaults).
        let result = registry().dispatch("man clear", &[], &[]);
        let joined = result
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.starts_with("clear —"));
        assert!(joined.contains("Usage:"));
        // Default usage is just the command name.
        assert!(joined.contains("  clear"));
        assert!(!joined.contains("Examples:"));
    }

    #[test]
    fn man_with_an_unknown_command_is_an_error() {
        let result = registry().dispatch("man nope", &[], &[]);
        assert_eq!(result.lines[0].kind, LineKind::Error);
        assert!(result.lines[0].text.contains("no manual entry"));
    }

    #[test]
    fn history_is_empty_when_nothing_was_entered() {
        let result = registry().dispatch("history", &[], &[]);
        assert_eq!(result.lines.len(), 1);
        assert!(result.lines[0].text.contains("No commands in history"));
    }

    #[test]
    fn history_numbers_previous_entries() {
        let entries = vec!["man".to_string(), "doctor".to_string()];
        let result = registry().dispatch("history", &entries, &[]);
        assert_eq!(result.lines.len(), 2);
        assert!(result.lines[0].text.contains("1"));
        assert!(result.lines[0].text.contains("man"));
        assert!(result.lines[1].text.contains("doctor"));
    }

    #[test]
    fn clear_requests_the_clear_effect_with_no_lines() {
        let result = registry().dispatch("clear", &[], &[]);
        assert!(result.lines.is_empty());
        assert_eq!(result.effect, Effect::Clear);
    }

    #[test]
    fn quit_and_exit_request_the_quit_effect() {
        assert_eq!(registry().dispatch("quit", &[], &[]).effect, Effect::Quit);
        assert_eq!(registry().dispatch("exit", &[], &[]).effect, Effect::Quit);
    }

    #[test]
    fn session_new_without_a_name_opens_the_modal() {
        // `session new` asks for a name via the modal.
        let result = registry().dispatch("session new", &[], &[]);
        assert!(result.lines.is_empty());
        assert_eq!(result.effect, Effect::OpenSessionModal);
    }

    #[test]
    fn session_new_with_a_name_requests_creation() {
        // Creation goes through `session new <name>` only.
        let result = registry().dispatch("session new feature-x", &[], &[]);
        assert!(result.lines.is_empty());
        assert_eq!(
            result.effect,
            Effect::CreateSession("feature-x".to_string())
        );
    }

    #[test]
    fn session_create_and_its_aliases_behave_like_new() {
        // `create` is the canonical name; `c` and `new` are aliases. Each opens
        // the modal with no name and creates with one.
        for sub in ["create", "c", "new"] {
            let opened = registry().dispatch(&format!("session {sub}"), &[], &[]);
            assert_eq!(opened.effect, Effect::OpenSessionModal, "{sub} (no name)");

            let created = registry().dispatch(&format!("session {sub} feature-x"), &[], &[]);
            assert_eq!(
                created.effect,
                Effect::CreateSession("feature-x".to_string()),
                "{sub} feature-x"
            );
        }
    }

    #[test]
    fn session_ls_is_an_alias_for_list() {
        let result = registry().dispatch("session ls", &[], &[]);
        assert!(result.lines.is_empty());
        assert_eq!(result.effect, Effect::ListSessions);
    }

    #[test]
    fn session_rm_is_an_alias_for_remove() {
        // `rm` with a name removes directly...
        let result = registry().dispatch("session rm old", &[], &[]);
        assert_eq!(
            result.effect,
            Effect::RemoveSession {
                name: "old".to_string(),
                force: false,
            }
        );
        // ...and `rm` with no name opens the picker, just like `remove`.
        let bare = registry().dispatch("session rm", &[], &[]);
        assert_eq!(bare.effect, Effect::OpenRemoveModal { force: false });
    }

    #[test]
    fn bare_session_and_the_old_name_shorthand_show_usage() {
        // Bare `session` and the removed `session <name>` shorthand no longer
        // create or open the modal; they fall through to a usage error.
        for input in ["session", "session feature-x"] {
            let result = registry().dispatch(input, &[], &[]);
            assert_eq!(result.effect, Effect::None);
            assert_eq!(result.lines.len(), 1);
            assert!(result.lines[0].text.contains("usage:"));
        }
    }

    #[test]
    fn session_list_requests_the_list_effect() {
        let result = registry().dispatch("session list", &[], &[]);
        assert!(result.lines.is_empty());
        assert_eq!(result.effect, Effect::ListSessions);
    }

    #[test]
    fn session_remove_parses_name_and_force_flag() {
        // A bare name removes without force.
        let result = registry().dispatch("session remove old", &[], &[]);
        assert!(result.lines.is_empty());
        assert_eq!(
            result.effect,
            Effect::RemoveSession {
                name: "old".to_string(),
                force: false,
            }
        );

        // `--force` (in any position) sets the force flag; extra positional
        // tokens after the name are ignored.
        for input in [
            "session remove old --force",
            "session remove -f old",
            "session remove old --force extra",
        ] {
            let result = registry().dispatch(input, &[], &[]);
            assert_eq!(
                result.effect,
                Effect::RemoveSession {
                    name: "old".to_string(),
                    force: true,
                }
            );
        }
    }

    #[test]
    fn session_remove_without_a_name_opens_the_removal_modal() {
        // A bare `session remove` opens the picker (no force).
        let result = registry().dispatch("session remove", &[], &[]);
        assert!(result.lines.is_empty());
        assert_eq!(result.effect, Effect::OpenRemoveModal { force: false });

        // `session remove --force` opens the picker carrying the force flag.
        let forced = registry().dispatch("session remove --force", &[], &[]);
        assert!(forced.lines.is_empty());
        assert_eq!(forced.effect, Effect::OpenRemoveModal { force: true });
    }

    #[test]
    fn coming_soon_commands_are_recognised() {
        let registry = registry();
        for name in ["ai", "doctor"] {
            let result = registry.dispatch(name, &[], &[]);
            assert_eq!(result.effect, Effect::None);
            assert_eq!(result.lines[0].kind, LineKind::Output);
            assert!(result.lines[0].text.contains("coming soon"));
            assert!(result.lines[0].text.contains(name));
        }
    }

    fn worktree_refs() -> Vec<WorktreeRef> {
        vec![
            WorktreeRef {
                name: "main".to_string(),
                active: true,
            },
            WorktreeRef {
                name: "feature".to_string(),
                active: false,
            },
        ]
    }

    #[test]
    fn session_switch_with_a_name_requests_activation() {
        let result = registry().dispatch("session switch feature", &[], &worktree_refs());
        assert_eq!(result.effect, Effect::Activate("feature".to_string()));
        // Resolution and messaging happen in the screen, so no lines here.
        assert!(result.lines.is_empty());
    }

    #[test]
    fn session_switch_without_a_name_enters_switch_mode() {
        // `session switch` with no name hands off to 切替 (Switch); the event loop
        // owns the mode transition, so no lines are produced here.
        let result = registry().dispatch("session switch", &[], &worktree_refs());
        assert_eq!(result.effect, Effect::EnterSwitch);
        assert!(result.lines.is_empty());
        // Even with no sessions it still enters Switch (the left pane has the root
        // row to pick or create from).
        assert_eq!(
            registry().dispatch("session switch", &[], &[]).effect,
            Effect::EnterSwitch
        );
    }

    #[test]
    fn terminal_requests_opening_a_shell() {
        let result = registry().dispatch("terminal", &[], &[]);
        assert!(result.lines.is_empty());
        assert_eq!(result.effect, Effect::OpenTerminal);
    }

    #[test]
    fn agent_requests_opening_the_agent() {
        let result = registry().dispatch("agent", &[], &[]);
        assert!(result.lines.is_empty());
        assert_eq!(result.effect, Effect::OpenAgent);
    }

    #[test]
    fn config_requests_opening_the_settings_screen() {
        let result = registry().dispatch("config", &[], &[]);
        assert!(result.lines.is_empty());
        assert_eq!(result.effect, Effect::OpenConfig);
    }

    #[test]
    fn unknown_command_is_reported_as_an_error() {
        let result = registry().dispatch("frobnicate", &[], &[]);
        assert_eq!(result.lines[0].kind, LineKind::Error);
        assert!(result.lines[0].text.contains("unknown command"));
    }

    #[test]
    fn registered_command_is_dispatchable_and_listed() {
        struct Greet;
        impl Command for Greet {
            fn name(&self) -> &'static str {
                "greet"
            }
            fn description(&self) -> &'static str {
                "Say hello"
            }
            fn run(&self, args: &str, _ctx: &CommandContext) -> CommandResult {
                CommandResult::line(LogLine::output(format!("hello {args}")))
            }
        }

        let mut registry = registry();
        registry.register(Box::new(Greet));
        let result = registry.dispatch("greet world", &[], &[]);
        assert_eq!(result.lines[0].text, "hello world");
        // The newcomer also shows up in `man` (via the shared info list).
        assert!(registry.infos().iter().any(|i| i.name == "greet"));
    }

    #[test]
    fn default_registry_matches_with_builtins() {
        assert_eq!(
            CommandRegistry::default().infos().len(),
            CommandRegistry::with_builtins().infos().len()
        );
    }

    #[test]
    fn complete_fills_in_a_unique_match() {
        // "doc" only matches "doctor" (a workspace command).
        let completion = registry().complete("doc", CommandScope::Workspace);
        assert_eq!(completion.input, "doctor");
        assert!(completion.candidates.is_empty());
    }

    #[test]
    fn complete_extends_to_the_common_prefix_and_lists_candidates() {
        // Register a second "s…" command so the prefix is ambiguous.
        struct Sync;
        impl Command for Sync {
            fn name(&self) -> &'static str {
                "sync"
            }
            fn description(&self) -> &'static str {
                "Sync"
            }
            fn run(&self, _args: &str, _ctx: &CommandContext) -> CommandResult {
                CommandResult::lines(Vec::new())
            }
        }
        let mut registry = registry();
        registry.register(Box::new(Sync));
        // The newcomer is fully wired (listed in `man`, dispatchable).
        assert!(registry.infos().iter().any(|i| i.name == "sync"));
        assert!(registry.dispatch("sync", &[], &[]).lines.is_empty());
        // "s" matches both "session" (workspace) and "sync" (a `Both` utility);
        // common prefix is "s". Completing in workspace scope offers both.
        let completion = registry.complete("s", CommandScope::Workspace);
        assert_eq!(completion.input, "s");
        assert_eq!(completion.candidates, vec!["session", "sync"]);
    }

    #[test]
    fn complete_with_no_match_leaves_input_untouched() {
        let completion = registry().complete("zzz", CommandScope::Workspace);
        assert_eq!(completion.input, "zzz");
        assert!(completion.candidates.is_empty());
    }

    #[test]
    fn complete_does_not_touch_input_with_arguments() {
        let completion = registry().complete("man ses", CommandScope::Workspace);
        assert_eq!(completion.input, "man ses");
        assert!(completion.candidates.is_empty());
    }

    #[test]
    fn complete_does_not_offer_aliases() {
        // "h" matches "history" but not the "help" alias.
        let completion = registry().complete("h", CommandScope::Workspace);
        assert_eq!(completion.input, "history");
        assert!(completion.candidates.is_empty());
    }

    #[test]
    fn common_prefix_handles_the_empty_case() {
        assert_eq!(common_prefix(&[]), "");
    }

    #[test]
    fn common_prefix_finds_the_shared_start() {
        assert_eq!(common_prefix(&["session", "space"]), "s");
        assert_eq!(common_prefix(&["terminal", "terminal"]), "terminal");
    }

    /// The command names offered for an empty input in `scope`. Completion lists
    /// every in-scope command when the input is empty, so its candidates are the
    /// scope's surface (avoiding an unreachable match arm on the hint enum).
    fn suggested_names(scope: CommandScope) -> Vec<String> {
        registry().complete("", scope).candidates
    }

    #[test]
    fn suggest_splits_the_command_surface_by_scope() {
        let has = |names: &[String], name: &str| names.iter().any(|n| n == name);

        // The 統括 (Overview) line offers the workspace commands and the shared
        // utilities, but never the session-specific ones.
        let workspace = suggested_names(CommandScope::Workspace);
        assert!(has(&workspace, "session"));
        assert!(has(&workspace, "config"));
        assert!(has(&workspace, "doctor"));
        assert!(has(&workspace, "man")); // a shared utility
        assert!(!has(&workspace, "terminal"));
        assert!(!has(&workspace, "agent"));
        assert!(!has(&workspace, "ai"));

        // The 在席 (Focus) prompt offers the session-specific commands and the
        // shared utilities, but never the workspace ones — the two surfaces are
        // physically separate, so they do not nest.
        let session = suggested_names(CommandScope::Session);
        assert!(has(&session, "terminal"));
        assert!(has(&session, "agent"));
        assert!(has(&session, "ai"));
        assert!(has(&session, "man")); // a shared utility
        assert!(!has(&session, "session"));
        assert!(!has(&session, "config"));
        assert!(!has(&session, "doctor"));
    }

    #[test]
    fn suggest_filters_commands_by_prefix() {
        // "s" only matches "session" in workspace scope.
        assert_eq!(
            registry().suggest("s", CommandScope::Workspace),
            Hint::Commands(vec![CommandHint {
                name: "session",
                description: "Create, list, or switch sessions (branch + worktree)",
            }])
        );
        // The scopes are separate, so "s" matches nothing in session scope (no
        // session-specific command begins with it).
        assert_eq!(registry().suggest("s", CommandScope::Session), Hint::None);
    }

    #[test]
    fn suggest_with_an_unknown_prefix_has_no_hint() {
        assert_eq!(
            registry().suggest("zzz", CommandScope::Workspace),
            Hint::None
        );
    }

    #[test]
    fn suggest_shows_usage_and_examples_once_arguments_are_typed() {
        // A trailing space moves past the command word onto its arguments.
        assert_eq!(
            registry().suggest("session ", CommandScope::Workspace),
            Hint::Usage {
                usage:
                    "session [create|list|switch|remove] <name>  (aliases: create=c/new, list=ls, remove=rm)",
                examples: &[
                    "session create feature-x",
                    "session switch feature-x",
                    "session ls",
                    "session rm feature-x",
                ],
            }
        );
    }

    #[test]
    fn suggest_with_arguments_to_an_unknown_command_has_no_hint() {
        assert_eq!(
            registry().suggest("frob bar", CommandScope::Workspace),
            Hint::None
        );
    }

    #[test]
    fn command_scope_visibility_is_same_scope_or_both() {
        // A command is offered in its own scope only; `Both` utilities everywhere.
        assert!(CommandScope::Workspace.visible_in(CommandScope::Workspace));
        assert!(!CommandScope::Workspace.visible_in(CommandScope::Session));
        assert!(CommandScope::Session.visible_in(CommandScope::Session));
        assert!(!CommandScope::Session.visible_in(CommandScope::Workspace));
        assert!(CommandScope::Both.visible_in(CommandScope::Workspace));
        assert!(CommandScope::Both.visible_in(CommandScope::Session));
    }

    #[test]
    fn commands_in_scope_lists_a_scopes_own_commands_in_order() {
        // The 在席 menu lists exactly the Session-scope commands, in registry
        // order, excluding the shared utilities. `terminal` comes first (and is
        // highlighted by default); the coming-soon `ai` placeholder comes last.
        let session: Vec<&str> = registry()
            .commands_in_scope(CommandScope::Session)
            .iter()
            .map(|i| i.name)
            .collect();
        assert_eq!(session, vec!["terminal", "agent", "ai"]);
        // Workspace scope lists its own commands and none of the session ones.
        let workspace: Vec<&str> = registry()
            .commands_in_scope(CommandScope::Workspace)
            .iter()
            .map(|i| i.name)
            .collect();
        assert!(workspace.contains(&"session"));
        assert!(workspace.contains(&"config"));
        assert!(!workspace.contains(&"terminal"));
    }

    #[test]
    fn complete_respects_the_current_scope() {
        // "a" matches the session commands "agent" and "ai" — offered in session
        // scope, in registration order (common prefix "a")…
        let session = registry().complete("a", CommandScope::Session);
        assert_eq!(session.input, "a");
        assert_eq!(session.candidates, vec!["agent", "ai"]);
        // …but nothing in workspace scope, so the input is left untouched.
        let workspace = registry().complete("a", CommandScope::Workspace);
        assert_eq!(workspace.input, "a");
        assert!(workspace.candidates.is_empty());
    }

    #[test]
    fn man_groups_commands_by_scope() {
        let joined = registry()
            .dispatch("man", &[], &[])
            .lines
            .iter()
            .map(|l| l.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        // The listing is split into the two scopes plus the shared utilities.
        assert!(joined.contains("Workspace (root):"));
        assert!(joined.contains("Session (selected):"));
        assert!(joined.contains("General:"));
        // Every command still appears under one of the groups.
        for info in registry().infos() {
            assert!(joined.contains(info.name));
        }
    }
}

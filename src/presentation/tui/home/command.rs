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
    /// Open the session-name modal (the user ran `session` without a name).
    OpenSessionModal,
    /// Create a session with the given name (the user supplied one).
    CreateSession(String),
    /// List the workspace's sessions (the user ran `session list`).
    ListSessions,
    /// Remove a session (the user ran `session remove <name> [--force]`).
    RemoveSession { name: String, force: bool },
    /// Switch the active worktree to the one named by the string. The screen
    /// resolves the name against its worktree list and reports the result.
    Activate(String),
    /// Open an interactive terminal in the selected worktree (the user ran
    /// `terminal`). The directory is resolved by the event loop.
    OpenTerminal,
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
            for info in ctx.commands {
                lines.push(LogLine::output(format!(
                    "  {:<9}{}",
                    info.name, info.description
                )));
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
/// - `session new <name>` (or just `session <name>`) creates a session; with no
///   name it returns [`Effect::OpenSessionModal`] so the screen can prompt.
/// - `session list` lists the sessions.
/// - `session switch <name>` switches the active session (via
///   [`Effect::Activate`]); `session switch` with no name lists the sessions and
///   marks the active one.
/// - `session remove <name> [--force]` removes a session.
struct SessionCommand;

impl Command for SessionCommand {
    fn name(&self) -> &'static str {
        "session"
    }

    fn description(&self) -> &'static str {
        "Create, list, or switch sessions (branch + worktree)"
    }

    fn usage(&self) -> &'static str {
        "session [new|list|switch|remove] <name>"
    }

    fn examples(&self) -> &'static [&'static str] {
        &[
            "session new feature-x",
            "session switch feature-x",
            "session list",
        ]
    }

    fn run(&self, args: &str, ctx: &CommandContext) -> CommandResult {
        let mut parts = args.splitn(2, char::is_whitespace);
        let sub = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("").trim();

        let open = || CommandResult {
            lines: Vec::new(),
            effect: Effect::OpenSessionModal,
        };
        let create = |name: &str| CommandResult {
            lines: Vec::new(),
            effect: Effect::CreateSession(name.to_string()),
        };

        match sub {
            "" => open(),
            "new" if rest.is_empty() => open(),
            "new" => create(rest),
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
                    Some(name) => CommandResult {
                        lines: Vec::new(),
                        effect: Effect::RemoveSession { name, force },
                    },
                    None => CommandResult::line(LogLine::error(
                        "usage: session remove <name> [--force]",
                    )),
                }
            }
            _ => create(args.trim()),
        }
    }
}

/// `session switch [name]`: switch the active session, or list the available
/// ones when no name is given.
///
/// With a name, the resolution (and the success/not-found message) happens in
/// the screen, which owns the worktree list and the active selection.
fn switch(name: &str, ctx: &CommandContext) -> CommandResult {
    if name.is_empty() {
        if ctx.worktrees.is_empty() {
            return CommandResult::line(LogLine::output("No sessions to switch between."));
        }
        let mut lines = vec![LogLine::output("Sessions:")];
        for worktree in ctx.worktrees {
            let marker = if worktree.active { "*" } else { " " };
            lines.push(LogLine::output(format!("  {marker} {}", worktree.name)));
        }
        lines.push(LogLine::output("Use \"session switch <name>\" to switch."));
        return CommandResult::lines(lines);
    }

    CommandResult {
        lines: Vec::new(),
        effect: Effect::Activate(name.to_string()),
    }
}

/// `terminal`: open an interactive shell in the selected worktree. The spawn is
/// a side effect ([`Effect::OpenTerminal`]) performed by the event loop, which
/// holds the worktree paths and the terminal handle.
struct TerminalCommand;

impl Command for TerminalCommand {
    fn name(&self) -> &'static str {
        "terminal"
    }

    fn description(&self) -> &'static str {
        "Open an interactive terminal in the selected worktree"
    }

    fn run(&self, _args: &str, _ctx: &CommandContext) -> CommandResult {
        CommandResult {
            lines: Vec::new(),
            effect: Effect::OpenTerminal,
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
    /// implemented feature commands (`session`, `space`, `ai`, `terminal`,
    /// `doctor`) are present as discoverable "coming soon" placeholders.
    pub fn with_builtins() -> Self {
        Self {
            commands: vec![
                Box::new(SessionCommand),
                Box::new(ComingSoonCommand {
                    name: "ai",
                    description: "Talk to the AI agent",
                    usage: "ai <prompt>",
                    examples: &["ai fix the failing test"],
                }),
                Box::new(TerminalCommand),
                Box::new(HistoryCommand),
                Box::new(ComingSoonCommand {
                    name: "doctor",
                    description: "Check that required tools are installed",
                    usage: "doctor",
                    examples: &[],
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
            })
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
    pub fn complete(&self, input: &str) -> Completion {
        if input.contains(char::is_whitespace) {
            return Completion {
                input: input.to_string(),
                candidates: Vec::new(),
            };
        }

        let matches: Vec<&str> = self
            .commands
            .iter()
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
        assert!(joined.contains("session [new|list|switch|remove] <name>"));
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
    fn session_without_a_name_opens_the_modal() {
        // Bare `session` and `session new` both ask for a name via the modal.
        for input in ["session", "session new"] {
            let result = registry().dispatch(input, &[], &[]);
            assert!(result.lines.is_empty());
            assert_eq!(result.effect, Effect::OpenSessionModal);
        }
    }

    #[test]
    fn session_with_a_name_requests_creation() {
        // `session new <name>` and the shorthand `session <name>` both create.
        for input in ["session new feature-x", "session feature-x"] {
            let result = registry().dispatch(input, &[], &[]);
            assert!(result.lines.is_empty());
            assert_eq!(
                result.effect,
                Effect::CreateSession("feature-x".to_string())
            );
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
    fn session_remove_without_a_name_shows_usage() {
        let result = registry().dispatch("session remove", &[], &[]);
        assert_eq!(result.effect, Effect::None);
        assert_eq!(result.lines[0].kind, LineKind::Error);
        assert!(result.lines[0].text.contains("usage"));
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
    fn session_switch_without_a_name_lists_sessions_and_marks_the_active_one() {
        let result = registry().dispatch("session switch", &[], &worktree_refs());
        assert_eq!(result.effect, Effect::None);
        let joined = result
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("Sessions:"));
        // The active session is marked with `*`.
        assert!(joined.contains("* main"));
        assert!(joined.contains("feature"));
        assert!(joined.contains("session switch <name>"));
    }

    #[test]
    fn session_switch_without_sessions_says_so() {
        let result = registry().dispatch("session switch", &[], &[]);
        assert_eq!(result.effect, Effect::None);
        assert!(result.lines[0].text.contains("No sessions"));
    }

    #[test]
    fn terminal_requests_opening_a_shell() {
        let result = registry().dispatch("terminal", &[], &[]);
        assert!(result.lines.is_empty());
        assert_eq!(result.effect, Effect::OpenTerminal);
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
        // "doc" only matches "doctor".
        let completion = registry().complete("doc");
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
        // "s" matches both "session" and "sync"; common prefix is "s".
        let completion = registry.complete("s");
        assert_eq!(completion.input, "s");
        assert_eq!(completion.candidates, vec!["session", "sync"]);
    }

    #[test]
    fn complete_with_no_match_leaves_input_untouched() {
        let completion = registry().complete("zzz");
        assert_eq!(completion.input, "zzz");
        assert!(completion.candidates.is_empty());
    }

    #[test]
    fn complete_does_not_touch_input_with_arguments() {
        let completion = registry().complete("man ses");
        assert_eq!(completion.input, "man ses");
        assert!(completion.candidates.is_empty());
    }

    #[test]
    fn complete_does_not_offer_aliases() {
        // "h" matches "history" but not the "help" alias.
        let completion = registry().complete("h");
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
}

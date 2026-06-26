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
//!
//! This module owns the command *vocabulary* — the [`Command`] trait and the
//! types commands exchange ([`Effect`], [`CommandResult`], [`CommandContext`],
//! [`Hint`], …). The built-in commands live in [`builtins`]; the
//! [`CommandRegistry`] that dispatches and completes them lives in [`registry`].

mod builtins;
mod registry;

pub use registry::CommandRegistry;

use super::state::LogLine;
use crate::domain::issue::Issue;
use crate::domain::settings::AgentCli;

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
    /// Open an AI agent in the selected worktree (the user ran `agent`). This is
    /// `terminal` with the agent CLI launched inside it; the directory and agent
    /// command are resolved by the event loop / wiring. The payload selects which
    /// CLI to launch: `None` uses the workspace's configured agent (the common
    /// fast path), `Some(cli)` overrides it for this launch (`agent <name>`).
    OpenAgent(Option<AgentCli>),
    /// Close (remove) the focused session (the user ran `close` in 在席). It is the
    /// session equivalent of `session remove <name>` (no `--force`): a clean
    /// session's worktrees/branches are deleted, but one with uncommitted changes
    /// is refused (the removal logs the `--force` hint). Either way 在席 is left for
    /// the base 切替 (Switch). The focused session's name is resolved by the event
    /// loop, which owns the worktree list and the removal callback.
    CloseSession,
    /// Open the configuration screen (the user ran `config`) to edit the global
    /// settings and this workspace's local overrides. The screen is run by the
    /// event loop, which returns to the workspace screen when it is dismissed.
    OpenConfig,
    /// Show the result lines in a scrollable text modal (rather than the results
    /// band), for commands whose output is text to read — `man` / `history`. The
    /// string is the modal title.
    ShowText(&'static str),
    /// Open the right-pane Markdown preview of the named file (the user ran
    /// `preview <path|name>`). The string is the requested target; the event loop
    /// resolves and reads it (under the workspace root) and renders it.
    OpenPreview(String),
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

    /// A result whose `lines` are shown in a scrollable text modal titled `title`
    /// (used by text-dumping commands like `man` / `history`) instead of the
    /// results band.
    fn modal(title: &'static str, lines: Vec<LogLine>) -> Self {
        Self {
            lines,
            effect: Effect::ShowText(title),
        }
    }
}

/// Which of the home screen's command scopes a command belongs to.
///
/// The two surfaces are *physically separate* in the home screen (切替 / 在席 /
/// 没入 with the `:` command palette over them, see `document/design/05-home.md`):
/// the `:` command palette operates the whole workspace
/// ([`CommandScope::Workspace`]), while the *在席 (Focus)* right pane operates one
/// session ([`CommandScope::Session`]). Because the two never share a line, the
/// scopes do not nest — a command is offered only in its own scope (plus the
/// shared utilities). Commands are offered (completion, hints, `man` grouping)
/// accordingly; [`CommandScope::Both`] commands are utilities available
/// everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandScope {
    /// Operating the whole workspace, the `:` command palette: `session`,
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

/// What a command may read while computing argument completions (Tab in the
/// middle of an argument string), built by the [`CommandRegistry`] for the
/// current scope. Kept separate from [`CommandContext`] because completion runs
/// on the bare input — before the workspace data a running command needs is
/// threaded in — and only needs the command vocabulary.
pub struct CompletionContext<'a> {
    /// Every registered command's primary name, in display order — what `man`
    /// completes its `[command]` argument against. Aliases are not offered, to
    /// match command-word completion.
    pub command_names: &'a [&'a str],
}

/// Everything a command may read while running, beyond its own argument string.
pub struct CommandContext<'a> {
    /// Commands entered so far this session (oldest first), for `history`.
    pub history: &'a [String],
    /// Every registered command, in display order, for `man`.
    pub commands: &'a [CommandInfo],
    /// The workspace's worktrees, in display order, for `space`.
    pub worktrees: &'a [WorktreeRef],
    /// The workspace's task issues, in number order, for `issue`.
    pub issues: &'a [Issue],
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

    /// Tab-completion candidates for this command's arguments, given everything
    /// typed after the command word (`args`).
    ///
    /// Returns the full set of tokens valid at the position of the *final*
    /// (still-being-typed) token — subcommands, flags, or other option words.
    /// The registry filters them by that token's prefix and fills in / lists
    /// them exactly as it does for command words, so implementors only describe
    /// their vocabulary, not the matching. Empty by default (no completable
    /// arguments).
    fn complete_args(&self, args: &str, ctx: &CompletionContext) -> Vec<String> {
        let _ = (args, ctx);
        Vec::new()
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

#[cfg(test)]
mod tests;

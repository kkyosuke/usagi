//! The built-in commands available in the workspace screen's command mode.
//! Each is a unit (or small) struct implementing [`Command`]; they are
//! registered in display order by [`super::CommandRegistry::with_builtins`].

use super::{Command, CommandContext, CommandInfo, CommandResult, CommandScope, Effect, LogLine};
use crate::domain::issue::IssueStatus;
use crate::usecase::issue::{annotate_all, dependency_tree, gantt, IssueStats, ListedIssue};

/// Line-width budget for the `issue gantt` chart, matching the text modal's
/// inner width so bars fill the box without being clipped.
const GANTT_WIDTH: usize = 60;

/// `man` / `help`: lists every command, or describes one.
pub(super) struct ManCommand;

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
            return CommandResult::modal("Help", lines);
        }

        match ctx.commands.iter().find(|info| info.name == args) {
            Some(info) => CommandResult::modal("Help", describe(info)),
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
pub(super) struct HistoryCommand;

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
        CommandResult::modal("History", lines)
    }
}

/// `clear`: empties the output pane.
pub(super) struct ClearCommand;

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
pub(super) struct QuitCommand;

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
pub(super) struct SessionCommand;

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
pub(super) struct TerminalCommand;

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
pub(super) struct AgentCommand;

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

/// `close`: remove the focused session forcefully and return to 統括 (Overview).
/// It is the 在席 equivalent of `session remove <name> --force` — the worktrees
/// and branches are deleted and any uncommitted changes discarded, so the
/// session is gone for good. The removal is a side effect ([`Effect::CloseSession`])
/// performed by the event loop, which owns the worktree list and the
/// session-removal callback.
pub(super) struct CloseCommand;

impl Command for CloseCommand {
    fn name(&self) -> &'static str {
        "close"
    }

    fn description(&self) -> &'static str {
        "Close the focused session (remove it, discarding any changes)"
    }

    fn scope(&self) -> CommandScope {
        CommandScope::Session
    }

    fn run(&self, _args: &str, _ctx: &CommandContext) -> CommandResult {
        CommandResult {
            lines: Vec::new(),
            effect: Effect::CloseSession,
        }
    }
}

/// `issue`: browse the workspace's task issues in a read-only modal — a list
/// with progress, the dependency tree, or one issue's full text. Mutating
/// issues stays the agent's job (via the MCP server), so this command only
/// reads what is already loaded into the context.
///
/// - `issue` / `issue list` (alias `ls`) — every issue with its readiness, plus
///   a progress summary.
/// - `issue graph` (alias `tree`) — the dependency forest.
/// - `issue gantt` (alias `chart`) — a date-axis Gantt chart of every issue's
///   `created_at`→`updated_at` span, annotated with dependencies.
/// - `issue show <number>` (alias `view`) — one issue's frontmatter and body.
pub(super) struct IssueCommand;

impl Command for IssueCommand {
    fn name(&self) -> &'static str {
        "issue"
    }

    fn description(&self) -> &'static str {
        "Browse task issues (list, graph, gantt, show)"
    }

    fn usage(&self) -> &'static str {
        "issue [list|graph|gantt|show <number>]  (aliases: list=ls, graph=tree, gantt=chart, show=view)"
    }

    fn examples(&self) -> &'static [&'static str] {
        &["issue", "issue graph", "issue gantt", "issue show 3"]
    }

    fn scope(&self) -> CommandScope {
        CommandScope::Workspace
    }

    fn run(&self, args: &str, ctx: &CommandContext) -> CommandResult {
        let mut parts = args.splitn(2, char::is_whitespace);
        let sub = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("").trim();

        let sub = match sub {
            "" | "list" | "ls" => "list",
            "graph" | "tree" => "graph",
            "gantt" | "chart" => "gantt",
            "show" | "view" => "show",
            other => other,
        };

        match sub {
            "list" => issue_list(ctx),
            "graph" => issue_graph(ctx),
            "gantt" => issue_gantt(ctx),
            "show" => issue_show(ctx, rest),
            _ => CommandResult::line(LogLine::error(format!("usage: {}", self.usage()))),
        }
    }
}

/// `issue list`: every issue with its readiness marker and a progress footer.
fn issue_list(ctx: &CommandContext) -> CommandResult {
    let listed = annotate_all(ctx.issues);
    if listed.is_empty() {
        return CommandResult::line(LogLine::output("No issues yet."));
    }
    let mut lines: Vec<LogLine> = listed.iter().map(issue_line).collect();
    lines.push(LogLine::output(String::new()));
    lines.push(LogLine::output(stats_line(&IssueStats::from_listed(
        &listed,
    ))));
    CommandResult::modal("Issues", lines)
}

/// `issue graph`: the dependency forest with a progress footer.
fn issue_graph(ctx: &CommandContext) -> CommandResult {
    let listed = annotate_all(ctx.issues);
    if listed.is_empty() {
        return CommandResult::line(LogLine::output("No issues yet."));
    }
    let mut lines: Vec<LogLine> = dependency_tree(&listed)
        .into_iter()
        .map(LogLine::output)
        .collect();
    lines.push(LogLine::output(String::new()));
    lines.push(LogLine::output(stats_line(&IssueStats::from_listed(
        &listed,
    ))));
    CommandResult::modal("Issue graph", lines)
}

/// `issue gantt`: a date-axis Gantt chart with a progress footer.
fn issue_gantt(ctx: &CommandContext) -> CommandResult {
    let listed = annotate_all(ctx.issues);
    if listed.is_empty() {
        return CommandResult::line(LogLine::output("No issues yet."));
    }
    let mut lines: Vec<LogLine> = gantt(&listed, GANTT_WIDTH)
        .into_iter()
        .map(LogLine::output)
        .collect();
    lines.push(LogLine::output(String::new()));
    lines.push(LogLine::output(stats_line(&IssueStats::from_listed(
        &listed,
    ))));
    CommandResult::modal("Issue gantt", lines)
}

/// `issue show <number>`: one issue's full markdown (frontmatter + body).
fn issue_show(ctx: &CommandContext, rest: &str) -> CommandResult {
    let Ok(number) = rest.parse::<u32>() else {
        return CommandResult::line(LogLine::error("usage: issue show <number>"));
    };
    match ctx.issues.iter().find(|i| i.number == number) {
        Some(issue) => {
            let lines = issue
                .to_markdown()
                .lines()
                .map(|l| LogLine::output(l.to_string()))
                .collect();
            CommandResult::modal("Issue", lines)
        }
        None => CommandResult::line(LogLine::error(format!("no issue #{number}"))),
    }
}

/// One aligned `#N status priority marker title` line for a listed issue.
fn issue_line(listed: &ListedIssue) -> LogLine {
    let marker = if listed.summary.status == IssueStatus::Done {
        "done"
    } else if listed.is_ready() {
        "ready"
    } else {
        "blocked"
    };
    let mut text = format!(
        "#{:<3} {:<12} {:<6} {:<8} {}",
        listed.summary.number,
        listed.summary.status.as_str(),
        listed.summary.priority.as_str(),
        marker,
        listed.summary.title,
    );
    if !listed.unmet_deps.is_empty() {
        let deps = listed
            .unmet_deps
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        text.push_str(&format!("  (blocked by {deps})"));
    }
    LogLine::output(text)
}

/// A one-line progress summary shown under issue listings and the graph.
fn stats_line(stats: &IssueStats) -> String {
    format!(
        "{} issues · {} done ({}%) · {} ready  {}",
        stats.total,
        stats.done,
        stats.completion_percent(),
        stats.ready,
        stats.progress_bar(20),
    )
}

/// `config`: open the configuration screen to edit **this workspace's** local
/// overrides (the global settings are edited from the CLI or welcome menu).
/// Opening the screen is a side effect ([`Effect::OpenConfig`]) performed by the
/// event loop, which owns the terminal and returns to the workspace screen when
/// the user dismisses it.
pub(super) struct ConfigCommand;

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
pub(super) struct ComingSoonCommand {
    pub(super) name: &'static str,
    pub(super) description: &'static str,
    pub(super) usage: &'static str,
    pub(super) examples: &'static [&'static str],
    pub(super) scope: CommandScope,
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

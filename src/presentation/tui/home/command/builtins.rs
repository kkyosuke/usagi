//! The built-in commands available in the workspace screen's command mode.
//! Each is a unit (or small) struct implementing [`Command`]; they are
//! registered in display order by [`super::CommandRegistry::with_builtins`].

use super::registry::arg_tokens;
use super::{
    Command, CommandContext, CommandInfo, CommandResult, CommandScope, CompletionContext, Effect,
    LogLine,
};
use crate::presentation::tui::widgets;
use crate::usecase::issue::{
    annotate_all, dependency_tree, gantt, list_line, stats_line, IssueStats, ListedIssue,
};

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
            return CommandResult::large_modal("Help", lines);
        }

        match ctx.commands.iter().find(|info| info.name == args) {
            Some(info) => CommandResult::large_modal("Help", describe(info)),
            None => CommandResult::line(LogLine::error(format!("no manual entry for \"{args}\""))),
        }
    }

    fn complete_args(&self, args: &str, ctx: &CompletionContext) -> Vec<String> {
        // `man [command]` takes a single command-name argument, so completion
        // offers every command name while the first token is being typed.
        let (head, _) = arg_tokens(args);
        if head.is_empty() {
            ctx.command_names.iter().map(|n| n.to_string()).collect()
        } else {
            Vec::new()
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
            lines: widgets::farewell_lines()
                .into_iter()
                .map(LogLine::output)
                .collect(),
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
        let sub = session_subcommand(sub);

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

    fn complete_args(&self, args: &str, ctx: &CompletionContext) -> Vec<String> {
        let (head, _) = arg_tokens(args);
        // Already-complete argument tokens after the subcommand word, e.g. a
        // session name or a flag the user has finished typing.
        let after_sub = || head.iter().skip(1);
        // Whether a session name has already been settled (any complete non-flag
        // token), so the `<name>` slot is filled and only flags remain.
        let name_chosen = || after_sub().any(|tok| !tok.starts_with('-'));
        let session_names = || ctx.session_names.iter().map(|n| n.to_string());

        match head.first().map(|sub| session_subcommand(sub)) {
            // Still on the subcommand word: offer the canonical subcommands.
            None => ["create", "list", "switch", "remove"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            // `session switch <name>` completes the session name; once one is
            // chosen there is nothing more to complete.
            Some("switch") if !name_chosen() => session_names().collect(),
            // `session remove <name> [--force]`: offer the session names (until
            // one is chosen) alongside the optional --force flag.
            Some("remove") => {
                let mut candidates: Vec<String> = if name_chosen() {
                    Vec::new()
                } else {
                    session_names().collect()
                };
                candidates.push("--force".to_string());
                candidates
            }
            // Other subcommands take a free-form name with nothing to complete.
            _ => Vec::new(),
        }
    }
}

/// Normalize a `session` subcommand alias to its canonical spelling (`c`/`new`
/// → `create`, `ls` → `list`, `rm` → `remove`), passing anything else through.
/// Shared by dispatch and completion so both honour the same aliases.
fn session_subcommand(sub: &str) -> &str {
    match sub {
        "create" | "c" | "new" => "create",
        "list" | "ls" => "list",
        "remove" | "rm" => "remove",
        other => other,
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

/// `agent [name]`: open an AI agent in the selected worktree. This is a shortcut
/// for running `terminal` and then launching the agent CLI inside it, so it
/// produces the [`Effect::OpenAgent`] side effect the event loop turns into an
/// embedded terminal with the agent command sent on start.
///
/// With no argument it launches the workspace's configured agent (the common
/// fast path); a name (`agent codex`, `agent sakana.ai`) overrides which CLI to
/// launch for this session. An unrecognised name is rejected with an error line;
/// whether a recognised CLI is actually installed is checked by the event loop
/// when it launches (it holds the PATH probe), so this command only parses.
pub(super) struct AgentCommand;

impl Command for AgentCommand {
    fn name(&self) -> &'static str {
        "agent"
    }

    fn description(&self) -> &'static str {
        "Open an AI agent in the selected session (terminal + agent CLI)"
    }

    fn usage(&self) -> &'static str {
        "agent [claude|codex|sakana.ai|gemini]"
    }

    fn examples(&self) -> &'static [&'static str] {
        &["agent", "agent codex", "agent sakana.ai"]
    }

    fn scope(&self) -> CommandScope {
        CommandScope::Session
    }

    fn run(&self, args: &str, _ctx: &CommandContext) -> CommandResult {
        let name = args.trim();
        // No name: launch the configured agent (the fast path).
        if name.is_empty() {
            return CommandResult {
                lines: Vec::new(),
                effect: Effect::OpenAgent(None),
            };
        }
        // A name overrides which CLI to launch; reject anything unrecognised.
        match crate::domain::settings::AgentCli::from_name(name) {
            Some(cli) => CommandResult {
                lines: Vec::new(),
                effect: Effect::OpenAgent(Some(cli)),
            },
            None => CommandResult::line(LogLine::error(format!(
                "unknown agent \"{name}\" (try {})",
                self.usage()
            ))),
        }
    }
}

/// `close`: remove the focused session and return to 切替 (Switch). It is the
/// 在席 equivalent of `session remove <name>` (no `--force`): a clean session's
/// worktrees and branches are deleted, but one with **uncommitted changes is
/// refused** — the removal logs how to discard them — so `close` can never
/// silently throw away unsaved work. The removal is a side effect
/// ([`Effect::CloseSession`]) performed by the event loop, which owns the
/// worktree list and the session-removal callback.
pub(super) struct CloseCommand;

impl Command for CloseCommand {
    fn name(&self) -> &'static str {
        "close"
    }

    fn description(&self) -> &'static str {
        "Close the focused session (remove it; kept if it has uncommitted changes)"
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

    fn complete_args(&self, args: &str, _ctx: &CompletionContext) -> Vec<String> {
        // Only the subcommand word completes; `show <number>` takes a free-form
        // issue number with nothing to offer.
        let (head, _) = arg_tokens(args);
        if head.is_empty() {
            ["list", "graph", "gantt", "show"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        } else {
            Vec::new()
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

/// One aligned `#N status priority marker title` line for a listed issue. The
/// layout lives in [`crate::usecase::issue::list_line`] so this command renders
/// identically to `usagi issue list`.
fn issue_line(listed: &ListedIssue) -> LogLine {
    LogLine::output(list_line(listed))
}

/// `preview`: render a Markdown file in the right pane (the third right-pane
/// state, alongside the command history/output and the live terminal). A path or
/// a bare name is accepted: `preview README` resolves to `README.md`. Reading the
/// file is the event loop's job, so this command only validates the argument and
/// returns [`Effect::OpenPreview`] carrying the target.
///
/// `preview diff` (the planned session-vs-main diff view) is not built yet; it is
/// recognised so the surface is discoverable, but only reports that for now.
pub(super) struct PreviewCommand;

impl Command for PreviewCommand {
    fn name(&self) -> &'static str {
        "preview"
    }

    fn description(&self) -> &'static str {
        "Preview a Markdown file in the right pane"
    }

    fn usage(&self) -> &'static str {
        "preview <path|name>"
    }

    fn examples(&self) -> &'static [&'static str] {
        &["preview README", "preview document/design/05-home.md"]
    }

    fn scope(&self) -> CommandScope {
        CommandScope::Workspace
    }

    fn run(&self, args: &str, _ctx: &CommandContext) -> CommandResult {
        let target = args.trim();
        if target.is_empty() {
            return CommandResult::line(LogLine::error(format!("usage: {}", self.usage())));
        }
        // The diff view is tracked separately and not implemented yet; recognise
        // it so the command reads coherently rather than trying to open `diff.md`.
        if target == "diff" {
            return CommandResult::line(LogLine::output(
                "Diff preview is coming soon 🐰 (for now, preview a Markdown file)",
            ));
        }
        CommandResult {
            lines: Vec::new(),
            effect: Effect::OpenPreview(target.to_string()),
        }
    }
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

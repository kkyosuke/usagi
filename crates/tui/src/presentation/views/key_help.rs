//! Contextual keyboard-help overlay.
//!
//! The input classifier owns the physical aliases for Help. This view owns only
//! the currently usable command vocabulary and renders it over an already-built
//! frame, so opening help cannot mutate the surface it describes.

use crate::presentation::theme::{Color, Style};
use crate::presentation::widgets::modal;
use crate::usecase::terminal_input::{PrefixHelpScope, prefix_help_entries};
use usagi_core::domain::settings::WorkMode;

/// Frontmost interaction surface whose commands should be shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    Welcome,
    Open,
    OpenUnregister,
    OpenCleanup,
    New,
    Config,
    TeamPicker,
    EnvironmentEditor,
    WorkspaceEnvironmentEditor,
    MissingWorkspace,
    Switch,
    Closeup,
    LiveTerminal,
    AddWorkspace,
    WorkspaceFinder,
    Overview,
    CloseupActions,
    CreateSession,
    CreateSessionError,
    TerminalLaunchError,
    AgentLaunchError,
    ExitConfirmation,
    ForceRemove,
    CleanupQueue,
    RemoveSessions,
    PullRequests,
    Preview,
    Scratchpad,
    RolesEditor,
    Daemon,
    DecisionList,
    DecisionAnswer,
    Organization,
    RunOverview,
    DirectorConsole,
    WorkRunConsole,
    DirectorNew,
    WorkRuns,
    WorkRunConfirmation,
    WorkRunSubmitting,
    WorkRunEscalation,
    RootShell,
    Garden,
}

impl Context {
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Welcome => "Welcome",
            Self::Open => "Open workspace",
            Self::OpenUnregister => "Unregister workspace",
            Self::OpenCleanup => "Cleanup registrations",
            Self::New => "New workspace",
            Self::Config => "Config",
            Self::TeamPicker => "Team picker",
            Self::EnvironmentEditor | Self::WorkspaceEnvironmentEditor => "Environment editor",
            Self::MissingWorkspace => "Missing workspace",
            Self::Switch => "Workspace switch",
            Self::Closeup => "Closeup",
            Self::LiveTerminal => "Live terminal",
            Self::AddWorkspace => "Add workspace",
            Self::WorkspaceFinder => "Project / session finder",
            Self::Overview => "Overview commands",
            Self::CloseupActions => "Closeup actions",
            Self::CreateSession => "Create session",
            Self::CreateSessionError => "Create session error",
            Self::TerminalLaunchError => "Terminal launch error",
            Self::AgentLaunchError => "Agent launch error",
            Self::ExitConfirmation => "Exit confirmation",
            Self::ForceRemove => "Force remove",
            Self::CleanupQueue => "Cleanup queue",
            Self::RemoveSessions => "Remove sessions",
            Self::PullRequests => "Pull Requests",
            Self::Preview => "File preview",
            Self::Scratchpad => "Scratchpad",
            Self::RolesEditor => "Roles editor",
            Self::Daemon => "Daemon control",
            Self::DecisionList => "Pending decisions",
            Self::DecisionAnswer => "Decision answer",
            Self::Organization => "Organization",
            Self::RunOverview => "Run Overview",
            Self::DirectorConsole | Self::WorkRunConsole => "Director Console",
            Self::DirectorNew => "New Conversation / Start Work Run",
            Self::WorkRuns => "Work Runs",
            Self::WorkRunConfirmation => "Work Run confirmation",
            Self::WorkRunSubmitting => "Work Run action in progress",
            Self::WorkRunEscalation => "Work Run escalation",
            Self::RootShell => "Workspace Shell",
            Self::Garden => "Session Garden",
        }
    }

    #[must_use]
    const fn workspace(self) -> bool {
        !matches!(
            self,
            Self::Welcome
                | Self::Open
                | Self::OpenUnregister
                | Self::OpenCleanup
                | Self::New
                | Self::Config
                | Self::TeamPicker
                | Self::EnvironmentEditor
                | Self::MissingWorkspace
        )
    }

    #[must_use]
    #[allow(clippy::too_many_lines)] // One exhaustive context-to-command catalog is easier to audit intact.
    pub const fn entries(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Welcome => &[
                ("↑/k  ↓/j", "select"),
                ("Enter", "open selected item"),
                ("o / e / c", "Open / New / Config"),
                ("1 … 3", "open Recent card"),
                ("q / Esc", "quit"),
            ],
            Self::Open => &[
                ("↑ / ↓", "select workspace"),
                ("type / paste", "edit filter"),
                ("Tab", "Single / Unite"),
                ("Space", "mark Unite member"),
                ("Ctrl-X", "unregister selected"),
                ("C", "clean missing registrations"),
                ("Enter / Esc", "open / back"),
            ],
            Self::OpenUnregister | Self::MissingWorkspace => &[
                ("← → / Tab", "choose remove or cancel"),
                ("y / Enter", "remove registration"),
                ("n / Esc", "cancel"),
            ],
            Self::OpenCleanup => &[("y / Enter", "clean registrations"), ("n / Esc", "cancel")],
            Self::New => &[
                ("↑ / ↓", "select field"),
                ("← / →", "mode or caret"),
                ("type / paste", "edit field"),
                ("Tab", "complete directory"),
                ("Enter / Esc", "create / back"),
            ],
            Self::Config => &[
                ("↑/k  ↓/j", "select setting"),
                ("←/h  →/l", "change value"),
                ("Enter", "open editor, picker, or save"),
                ("Esc", "back"),
            ],
            Self::TeamPicker => &[
                ("←/h  →/l", "select template card"),
                ("↑/k  ↓/j", "template / no template"),
                ("Enter / Esc", "apply / cancel"),
            ],
            Self::EnvironmentEditor | Self::WorkspaceEnvironmentEditor => &[
                ("type / paste", "edit source"),
                ("arrows / Home / End", "move caret"),
                ("Enter", "newline"),
                ("Tab", "textarea / Save"),
                ("Ctrl-S", "save"),
                ("Esc", "cancel"),
            ],
            Self::Switch => &[
                ("↑ / ↓", "select session"),
                ("← / →", "previous / next project"),
                ("Ctrl+Option+↑ / ↓", "previous / next session"),
                ("Ctrl+Option+← / →", "previous / next project"),
                ("Enter / t", "open Closeup"),
                ("Ctrl-A / Home", "new session"),
                (":", "Overview commands"),
                ("?", "keyboard shortcuts"),
                ("Ctrl-X", "remove session / purge orphan"),
                ("Ctrl-Q", "leave / quit prompt"),
            ],
            Self::Closeup => &[
                ("a / t", "open Agent / Terminal"),
                ("Enter", "open Action menu"),
                ("Ctrl+Option+↑ / ↓", "previous / next session"),
                ("Ctrl+Option+← / →", "previous / next project"),
                ("Ctrl-O [ / ]", "select pane tab"),
                ("Ctrl-O { / }", "reorder pane tab"),
                ("Ctrl-O x / r", "close / resume tab"),
                ("Ctrl-O o", "back to Switch"),
                ("?", "keyboard shortcuts"),
            ],
            Self::LiveTerminal => &[
                ("type / paste", "send to terminal"),
                ("Ctrl-C / Ctrl-D", "interrupt / EOT"),
                ("Ctrl+Option+↑ / ↓", "previous / next session"),
                ("Ctrl+Option+← / →", "previous / next project"),
                ("Ctrl-O [ / ]", "select pane tab"),
                ("Ctrl-O { / }", "reorder pane tab"),
                ("Ctrl-O x", "close pane tab"),
                ("Ctrl-O ↑ / ↓ / End", "scroll / live bottom"),
                ("Ctrl-O o", "back to Switch"),
                ("Ctrl-O ?", "keyboard shortcuts"),
            ],
            Self::AddWorkspace => &[
                ("Tab", "registered / directory"),
                ("↑ / ↓", "select registered project"),
                ("type / paste", "edit filter or path"),
                ("Space", "mark project"),
                ("Ctrl-X", "detach open project"),
                ("Enter / Esc", "add / cancel"),
            ],
            Self::WorkspaceFinder => &[
                ("↑ / ↓", "select project or session"),
                ("type / paste", "edit fuzzy filter"),
                ("1 … 9", "open project directly"),
                ("Ctrl-X", "detach project row"),
                ("Enter / Esc", "open / cancel"),
            ],
            Self::Overview => &[
                ("↑ / ↓", "candidate / history"),
                ("← / →", "move caret"),
                ("type / paste", "edit command"),
                ("Tab / Enter", "complete / run"),
                ("Esc", "close"),
            ],
            Self::CloseupActions => &[
                ("↑ / ↓", "select action"),
                ("← / →", "collapse / expand"),
                ("type / paste", "edit arguments"),
                ("Tab / Enter", "complete / run"),
                ("Esc", "close"),
            ],
            Self::CreateSession => &[
                ("type / paste", "edit session name"),
                ("↑ / ↓", "select base branch"),
                ("Tab", "select role"),
                ("Enter / Esc", "create / cancel"),
            ],
            Self::CreateSessionError | Self::TerminalLaunchError | Self::AgentLaunchError => {
                &[("Enter / Esc / Ctrl-C", "dismiss")]
            }
            Self::ExitConfirmation => &[
                ("← → / Tab", "select choice"),
                ("w", "return to Welcome"),
                ("q / y", "quit"),
                ("n / Esc", "stay"),
                ("Enter", "confirm selected choice"),
            ],
            Self::ForceRemove => &[
                ("← → / Tab", "select Yes / No"),
                ("y / Enter", "force remove"),
                ("n / Esc", "cancel"),
            ],
            Self::CleanupQueue => &[
                ("↑ / ↓", "select session"),
                ("Space", "mark session"),
                ("a / A", "select all / none"),
                ("Enter / Esc", "remove / close"),
            ],
            Self::RemoveSessions => &[
                ("↑/k  ↓/j", "select session"),
                ("Space", "mark session"),
                ("Enter", "remove selected"),
                ("Esc", "cancel"),
            ],
            Self::PullRequests => &[
                ("← / →", "select status"),
                ("↑ / ↓", "select Pull Request"),
                ("c", "copy URL"),
                ("Ctrl-X", "dismiss selected"),
                ("Enter / Esc", "open in browser / close"),
            ],
            Self::Preview => &[
                ("type / paste", "edit fuzzy filter"),
                ("↑ / ↓", "select file / scroll"),
                ("Enter", "preview selected file"),
                ("Esc", "back / close"),
            ],
            Self::Scratchpad => &[("paste", "append to draft"), ("Esc", "close")],
            Self::RolesEditor => &[
                ("Tab", "global / workspace scope"),
                ("↑ ↓ / PgUp PgDn", "move by row / page"),
                ("type / paste / Enter", "edit source"),
                ("Ctrl-S", "save"),
                ("Esc", "close"),
            ],
            Self::Daemon => &[
                ("↑ ↓ / ← → / Tab", "select action"),
                ("s / r / x", "start / restart / stop"),
                ("Enter", "run selected action"),
                ("Esc", "close"),
            ],
            Self::DecisionList => &[
                ("↑ / ↓", "select decision"),
                ("Enter / Esc", "answer / close"),
            ],
            Self::DecisionAnswer => &[
                ("↑ / ↓", "select option"),
                ("PgUp / PgDn", "scroll prompt"),
                ("type / paste", "edit freeform answer"),
                ("Enter / Esc", "submit / back to list"),
            ],
            Self::Organization => &[
                ("↑ / ↓", "select conversation"),
                ("Enter", "open its Director Console"),
                ("Ctrl-O n", "new conversation"),
                ("Esc", "close Director"),
            ],
            Self::RunOverview => &[
                ("Enter", "open Director Console"),
                ("Ctrl-C / Ctrl-X", "cancel / delete run"),
                ("Esc / Ctrl-O b", "back to Work Runs"),
                ("Ctrl-O w", "open Work Runs"),
                ("Ctrl-O n", "start Work Run"),
            ],
            Self::DirectorConsole => &[
                ("type / paste / Enter / Esc", "send directly to Agent PTY"),
                ("Ctrl-O [ / ]", "select conversation"),
                ("Ctrl-O b", "back to Organization"),
                ("Ctrl-O n", "new conversation"),
                ("Ctrl-O x / r", "close / resume"),
                ("Ctrl-O ↑ / ↓ / End", "scroll / live bottom"),
                ("Ctrl-O g", "close Director"),
            ],
            Self::WorkRunConsole => &[
                ("type / paste / Enter / Esc", "send directly to Agent PTY"),
                ("Ctrl-O b", "back to Run Overview"),
                ("Ctrl-O w", "open Work Runs"),
                ("Ctrl-O n", "start Work Run"),
                ("Ctrl-O x / r", "close / resume"),
                ("Ctrl-O ↑ / ↓ / End", "scroll / live bottom"),
                ("Ctrl-O g", "close Director"),
            ],
            Self::DirectorNew => &[
                ("↑ / ↓", "select provider"),
                ("type / paste", "edit goal when shown"),
                ("Enter / Esc / Ctrl-C", "launch / cancel"),
            ],
            Self::WorkRuns => &[
                ("↑ / ↓", "select run"),
                ("Enter", "open Run Overview"),
                ("Ctrl-C / Ctrl-X", "cancel / delete run"),
                ("Ctrl-O n", "start Work Run"),
                ("Esc", "close Director"),
            ],
            Self::WorkRunConfirmation => &[("Enter / Esc / Ctrl-C", "confirm / back")],
            Self::WorkRunSubmitting => &[("Ctrl-O g", "close Director; action continues")],
            Self::WorkRunEscalation => &[
                ("arrows", "select resolution"),
                ("Enter / Esc", "confirm / back"),
            ],
            Self::RootShell => &[
                ("type / paste", "send to shell"),
                ("Ctrl-O n", "new terminal tab"),
                ("Ctrl-O [ / ]", "select terminal tab"),
                ("Ctrl-O z / x", "resize / close tab"),
                ("Ctrl-O ↑ / ↓ / End", "scroll / live bottom"),
                ("Ctrl-O t", "close Shell"),
            ],
            Self::Garden => &[("← / →", "pan"), ("any other key", "wake and close")],
        }
    }

    #[must_use]
    const fn workspace_base(self) -> bool {
        matches!(self, Self::Switch | Self::Closeup | Self::LiveTerminal)
    }
}

/// Stable state for a keyboard-help overlay. The context and feature mode are
/// captured when Help opens so background updates cannot change its contents;
/// only the reader-controlled viewport offset changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct State {
    context: Context,
    work_mode: WorkMode,
    offset: usize,
}

impl State {
    #[must_use]
    pub const fn new(context: Context, work_mode: WorkMode) -> Self {
        Self {
            context,
            work_mode,
            offset: 0,
        }
    }

    #[must_use]
    pub const fn context(self) -> Context {
        self.context
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.offset = self.offset.saturating_sub(lines);
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.offset = self.offset.saturating_add(lines);
    }

    pub fn scroll_home(&mut self) {
        self.offset = 0;
    }

    pub fn scroll_end(&mut self) {
        self.offset = usize::MAX;
    }
}

/// Render contextual commands over `base` without changing the described
/// surface. ANSI-safe compositing and narrow-terminal clipping are delegated to
/// the shared modal widget.
#[must_use]
pub fn render_over(height: usize, width: usize, base: &[String], state: State) -> Vec<String> {
    let context = state.context;
    let mut commands = context.entries().to_vec();
    if context.workspace() {
        commands.extend(
            prefix_help_entries(PrefixHelpScope::Workspace, state.work_mode)
                .map(|entry| (entry.keys, entry.action)),
        );
    }
    if context.workspace_base() {
        commands.extend(
            prefix_help_entries(PrefixHelpScope::WorkspaceBase, state.work_mode)
                .map(|entry| (entry.keys, entry.action)),
        );
    }
    let key_width = commands
        .iter()
        .map(|(keys, _)| keys.chars().count())
        .max()
        .unwrap_or(0)
        .min(22);
    let command_rows = commands
        .into_iter()
        .map(|(keys, action)| {
            format!(
                "{}  {}",
                Style::new()
                    .fg(Color::Cyan)
                    .bold()
                    .paint(&format!("{keys:key_width$}")),
                Style::new().fg(Color::White).paint(action),
            )
        })
        .collect::<Vec<_>>();
    let body_capacity = modal::reserved_body_height(height, width, command_rows.len() + 2);
    let command_capacity = body_capacity.saturating_sub(2);
    let mut body = bounded_command_rows(&command_rows, state.offset, command_capacity);
    if body_capacity > 1 {
        body.push(String::new());
    }
    let close_hint = match context {
        Context::Switch | Context::Closeup => "? / Ctrl-? / Ctrl-/ or Esc: close help",
        Context::LiveTerminal => "Ctrl-O ? / Ctrl-? / Ctrl-/ or Esc: close help",
        _ => "Ctrl-? / Ctrl-/ or Esc: close help",
    };
    if body_capacity > 0 {
        let close_hint = if command_rows.len() > command_capacity {
            format!("↑/↓ PgUp/PgDn: scroll · {close_hint}")
        } else {
            close_hint.to_owned()
        };
        body.push(Style::new().fg(Color::White).dim().paint(&close_hint));
    }
    modal::render_over(
        height,
        width,
        base,
        &format!("Keyboard help · {}", context.title()),
        84,
        &body,
    )
}

fn bounded_command_rows(rows: &[String], offset: usize, capacity: usize) -> Vec<String> {
    if capacity == 0 || rows.is_empty() {
        return Vec::new();
    }
    // End/PageDown must land on a full final page. Reserving one row for the
    // "above" indicator leaves `capacity - 1` data rows at the bottom.
    let max_start = if rows.len() > capacity {
        rows.len().saturating_sub(capacity.saturating_sub(1))
    } else {
        0
    };
    let start = offset.min(max_start);
    let above = usize::from(start > 0);
    let available_after_above = capacity.saturating_sub(above);
    let below = usize::from(rows.len().saturating_sub(start) > available_after_above);
    let data_capacity = capacity.saturating_sub(above + below);
    let end = start.saturating_add(data_capacity).min(rows.len());
    let mut visible = Vec::with_capacity(capacity);
    if above > 0 {
        visible.push(modal::scroll_above(start));
    }
    visible.extend(rows[start..end].iter().cloned());
    if end < rows.len() {
        visible.push(modal::scroll_below(rows.len() - end));
    }
    visible
}

#[cfg(test)]
mod tests {
    use super::{Context, State, bounded_command_rows, render_over};
    use usagi_core::domain::settings::WorkMode;

    fn help(context: Context) -> State {
        State::new(context, WorkMode::GoalDriven)
    }

    #[test]
    fn every_context_has_a_renderable_title_and_command_catalog() {
        for context in [
            Context::Welcome,
            Context::Open,
            Context::OpenUnregister,
            Context::OpenCleanup,
            Context::New,
            Context::Config,
            Context::TeamPicker,
            Context::EnvironmentEditor,
            Context::WorkspaceEnvironmentEditor,
            Context::MissingWorkspace,
            Context::Switch,
            Context::Closeup,
            Context::LiveTerminal,
            Context::AddWorkspace,
            Context::WorkspaceFinder,
            Context::Overview,
            Context::CloseupActions,
            Context::CreateSession,
            Context::CreateSessionError,
            Context::TerminalLaunchError,
            Context::AgentLaunchError,
            Context::ExitConfirmation,
            Context::ForceRemove,
            Context::CleanupQueue,
            Context::RemoveSessions,
            Context::PullRequests,
            Context::Preview,
            Context::Scratchpad,
            Context::RolesEditor,
            Context::Daemon,
            Context::DecisionList,
            Context::DecisionAnswer,
            Context::Organization,
            Context::RunOverview,
            Context::DirectorConsole,
            Context::WorkRunConsole,
            Context::DirectorNew,
            Context::WorkRuns,
            Context::WorkRunConfirmation,
            Context::WorkRunSubmitting,
            Context::WorkRunEscalation,
            Context::RootShell,
            Context::Garden,
        ] {
            assert!(!context.title().is_empty(), "{context:?}");
            assert!(!context.entries().is_empty(), "{context:?}");

            let rendered = render_over(96, 120, &vec![String::new(); 96], help(context)).join("\n");
            assert!(rendered.contains(context.title()), "{context:?}");
        }
    }

    #[test]
    fn renders_only_the_frontmost_context_with_portable_close_hint() {
        let frame = render_over(
            24,
            100,
            &vec!["base".to_owned(); 24],
            help(Context::PullRequests),
        );
        let rendered = frame.join("\n");

        assert!(rendered.contains("Keyboard help · Pull Requests"));
        assert!(rendered.contains("Ctrl-X"));
        assert!(rendered.contains("dismiss selected"));
        assert!(rendered.contains("Ctrl-? / Ctrl-/ or Esc"));
        assert!(!rendered.contains("safe-remove session"));
    }

    #[test]
    fn entry_help_omits_workspace_only_commands() {
        let frame = render_over(18, 80, &vec![String::new(); 18], help(Context::Welcome));
        let rendered = frame.join("\n");

        assert!(rendered.contains("open Recent card"));
        assert!(!rendered.contains("add workspace"));
    }

    #[test]
    fn workspace_base_help_includes_launchers_but_modal_help_does_not() {
        let base = render_over(30, 100, &vec![String::new(); 30], help(Context::Switch)).join("\n");
        let modal = render_over(
            30,
            100,
            &vec![String::new(); 30],
            help(Context::PullRequests),
        )
        .join("\n");

        assert!(base.contains("Pull Requests / Preview"));
        assert!(modal.contains("activate project"));
        assert!(!modal.contains("Pull Requests / Preview"));
    }

    #[test]
    fn workspace_environment_help_keeps_process_level_project_commands() {
        let frame = render_over(
            30,
            100,
            &vec![String::new(); 30],
            help(Context::WorkspaceEnvironmentEditor),
        )
        .join("\n");

        assert!(frame.contains("edit source"));
        assert!(frame.contains("project / session finder"));
    }

    #[test]
    fn classic_help_omits_goal_only_work_runs() {
        let classic = render_over(
            40,
            120,
            &vec![String::new(); 40],
            State::new(Context::Switch, WorkMode::Classic),
        )
        .join("\n");
        let goal_driven = render_over(
            40,
            120,
            &vec![String::new(); 40],
            State::new(Context::Switch, WorkMode::GoalDriven),
        )
        .join("\n");

        assert!(!classic.contains("Work Runs"));
        assert!(goal_driven.contains("Work Runs"));
    }

    #[test]
    fn short_help_keeps_close_and_scroll_controls_visible() {
        let mut state = help(Context::Switch);
        let first = render_over(18, 80, &vec![String::new(); 18], state).join("\n");
        assert!(first.contains("close help"));
        assert!(first.contains("↓"));
        assert!(first.contains("PgUp/PgDn"));

        state.scroll_end();
        let last = render_over(18, 80, &vec![String::new(); 18], state).join("\n");
        assert!(last.contains("close help"));
        assert!(last.contains("↑"));
        assert!(last.contains("Work Runs"));
    }

    #[test]
    fn zero_capacity_help_omits_body_rows_without_panicking() {
        assert!(bounded_command_rows(&["row".to_owned()], 0, 0).is_empty());
        assert!(bounded_command_rows(&[], 0, 3).is_empty());

        let rendered = render_over(5, 80, &vec![String::new(); 5], help(Context::Switch));
        assert!(!rendered.join("\n").contains("close help"));
    }

    #[test]
    fn end_scroll_fills_the_last_page_instead_of_showing_only_the_last_row() {
        let rows = (0..8)
            .map(|index| format!("row {index}"))
            .collect::<Vec<_>>();
        let visible = bounded_command_rows(&rows, usize::MAX, 4);

        assert_eq!(visible.len(), 4);
        assert!(visible[0].contains('↑'));
        assert_eq!(&visible[1..], &["row 5", "row 6", "row 7"]);
    }
}

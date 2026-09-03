//! Contextual keyboard-help overlay.
//!
//! The input classifier owns the physical aliases for Help. This view owns only
//! the currently usable command vocabulary and renders it over an already-built
//! frame, so opening help cannot mutate the surface it describes.

use crate::presentation::theme::{Color, Style};
use crate::presentation::widgets::modal;

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
    ExitConfirmation,
    ForceRemove,
    CleanupQueue,
    PullRequests,
    Preview,
    Scratchpad,
    RolesEditor,
    Daemon,
    DecisionList,
    DecisionAnswer,
    Director,
    DirectorNew,
    WorkRuns,
    WorkRunConfirmation,
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
            Self::ExitConfirmation => "Exit confirmation",
            Self::ForceRemove => "Force remove",
            Self::CleanupQueue => "Cleanup queue",
            Self::PullRequests => "Pull Requests",
            Self::Preview => "Markdown preview",
            Self::Scratchpad => "Scratchpad",
            Self::RolesEditor => "Roles editor",
            Self::Daemon => "Daemon status",
            Self::DecisionList => "Pending decisions",
            Self::DecisionAnswer => "Decision answer",
            Self::Director => "Director conversation",
            Self::DirectorNew => "Director New",
            Self::WorkRuns => "Work Runs",
            Self::WorkRunConfirmation => "Work Run confirmation",
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
                ("Enter / t", "open Closeup"),
                ("Ctrl-A / Home", "new session"),
                (":", "Overview commands"),
                ("Ctrl-X", "safe-remove session"),
                ("Ctrl-Q", "leave / quit prompt"),
            ],
            Self::Closeup => &[
                ("a / t", "open Agent / Terminal"),
                ("Enter", "open Action menu"),
                ("Ctrl-O [ / ]", "select pane tab"),
                ("Ctrl-O { / }", "reorder pane tab"),
                ("Ctrl-O x / r", "close / resume tab"),
                ("Ctrl-O o", "back to Switch"),
            ],
            Self::LiveTerminal => &[
                ("type / paste", "send to terminal"),
                ("Ctrl-C / Ctrl-D", "interrupt / EOT"),
                ("Ctrl-O [ / ]", "select pane tab"),
                ("Ctrl-O { / }", "reorder pane tab"),
                ("Ctrl-O x", "close pane tab"),
                ("Ctrl-O ↑ / ↓ / End", "scroll / live bottom"),
                ("Ctrl-O o", "back to Switch"),
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
            Self::CreateSessionError => &[("Enter / Esc / Ctrl-C", "dismiss")],
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
            Self::PullRequests => &[
                ("← / →", "select status"),
                ("↑ / ↓", "select Pull Request"),
                ("c", "copy URL"),
                ("Ctrl-X", "dismiss selected"),
                ("Enter / Esc", "open in browser / close"),
            ],
            Self::Preview => &[("↑ / ↓", "scroll"), ("Esc", "close")],
            Self::Scratchpad => &[("paste", "append to draft"), ("Esc", "close")],
            Self::RolesEditor => &[
                ("Tab", "global / workspace scope"),
                ("↑ ↓ / PgUp PgDn", "move by row / page"),
                ("type / paste / Enter", "edit source"),
                ("Ctrl-S", "save"),
                ("Esc", "close"),
            ],
            Self::Daemon => &[("Esc", "close")],
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
            Self::Director => &[
                ("Ctrl-O [ / ]", "select conversation"),
                ("Ctrl-O n", "new conversation"),
                ("Ctrl-O x / r", "close / resume"),
                ("Ctrl-O ↑ / ↓ / End", "scroll / live bottom"),
                ("Ctrl-O g / Esc", "close Director"),
            ],
            Self::DirectorNew => &[
                ("↑ / ↓", "select provider"),
                ("type / paste", "edit goal when shown"),
                ("Enter / Esc", "launch / cancel"),
            ],
            Self::WorkRuns => &[
                ("↑ / ↓", "select run"),
                ("← / →", "previous / next run"),
                ("Enter / Esc", "actions / close"),
            ],
            Self::WorkRunConfirmation => &[("Enter / Esc", "confirm / back")],
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

const WORKSPACE_COMMANDS: &[(&str, &str)] = &[
    ("Ctrl-O +", "add workspace"),
    ("Ctrl-O 0", "project / session finder"),
    ("Ctrl-O 1 … 9", "activate project"),
];

const WORKSPACE_BASE_COMMANDS: &[(&str, &str)] = &[
    ("Ctrl-O a / n", "actions / Director New"),
    ("Ctrl-O p / v", "Pull Requests / Preview"),
    ("Ctrl-O d / s", "Decisions / Scratchpad"),
    (
        "Ctrl-O , / g / w / t",
        "Garden / Director / Work Runs / Shell",
    ),
];

/// Render contextual commands over `base` without changing the described
/// surface. ANSI-safe compositing and narrow-terminal clipping are delegated to
/// the shared modal widget.
#[must_use]
pub fn render_over(height: usize, width: usize, base: &[String], context: Context) -> Vec<String> {
    let mut commands = context.entries().to_vec();
    if context.workspace() {
        commands.extend_from_slice(WORKSPACE_COMMANDS);
    }
    if context.workspace_base() {
        commands.extend_from_slice(WORKSPACE_BASE_COMMANDS);
    }
    let key_width = commands
        .iter()
        .map(|(keys, _)| keys.chars().count())
        .max()
        .unwrap_or(0)
        .min(22);
    let mut body = commands
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
    body.push(String::new());
    body.push(
        Style::new()
            .fg(Color::White)
            .dim()
            .paint("Ctrl-? / Ctrl-/ or Esc: close help"),
    );
    modal::render_over(
        height,
        width,
        base,
        &format!("Keyboard help · {}", context.title()),
        66,
        &body,
    )
}

#[cfg(test)]
mod tests {
    use super::{Context, render_over};

    #[test]
    fn renders_only_the_frontmost_context_with_portable_close_hint() {
        let frame = render_over(24, 100, &vec!["base".to_owned(); 24], Context::PullRequests);
        let rendered = frame.join("\n");

        assert!(rendered.contains("Keyboard help · Pull Requests"));
        assert!(rendered.contains("Ctrl-X"));
        assert!(rendered.contains("dismiss selected"));
        assert!(rendered.contains("Ctrl-? / Ctrl-/ or Esc"));
        assert!(!rendered.contains("safe-remove session"));
    }

    #[test]
    fn entry_help_omits_workspace_only_commands() {
        let frame = render_over(18, 80, &vec![String::new(); 18], Context::Welcome);
        let rendered = frame.join("\n");

        assert!(rendered.contains("open Recent card"));
        assert!(!rendered.contains("add workspace"));
    }

    #[test]
    fn workspace_base_help_includes_launchers_but_modal_help_does_not() {
        let base = render_over(30, 100, &vec![String::new(); 30], Context::Switch).join("\n");
        let modal =
            render_over(30, 100, &vec![String::new(); 30], Context::PullRequests).join("\n");

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
            Context::WorkspaceEnvironmentEditor,
        )
        .join("\n");

        assert!(frame.contains("edit source"));
        assert!(frame.contains("project / session finder"));
    }
}

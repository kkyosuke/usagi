//! Context-aware command help opened with `?`.
//!
//! The modal is a read-only projection of the existing Overview and Closeup
//! command registries. `Available` keeps only commands runnable in the current
//! Home scope, while `All` keeps the complete registry and marks commands that
//! cannot run right now. The registries remain the single source of command
//! names, usage, and descriptions.

use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::modal;
use crate::usecase::{closeup, overview};

const INNER_WIDTH: usize = 76;
const BODY_HEIGHT: usize = 20;
const FIXED_ROWS: usize = 4;

/// Home scope from which command help was opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandScope {
    Workspace,
    Session,
}

impl CommandScope {
    const fn label(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::Session => "session",
        }
    }
}

/// Facts that decide whether a registered command can run in the current frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandHelpContext {
    pub scope: CommandScope,
    pub garden_available: bool,
    pub agent_available: bool,
    pub session_available: bool,
}

/// The two command-list tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandHelpTab {
    Available,
    All,
}

impl CommandHelpTab {
    const TABS: [Self; 2] = [Self::Available, Self::All];

    const fn index(self) -> usize {
        match self {
            Self::Available => 0,
            Self::All => 1,
        }
    }

    const fn toggled(self) -> Self {
        match self {
            Self::Available => Self::All,
            Self::All => Self::Available,
        }
    }
}

/// One normalized row from either command registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandHelpEntry {
    scope: CommandScope,
    name: &'static str,
    description: &'static str,
    usage: &'static str,
    available: bool,
}

impl CommandHelpEntry {
    #[must_use]
    pub const fn scope(&self) -> CommandScope {
        self.scope
    }

    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    #[must_use]
    pub const fn usage(&self) -> &'static str {
        self.usage
    }

    #[must_use]
    pub const fn is_available(&self) -> bool {
        self.available
    }
}

/// Pure state for the command-help tabs and their list cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandHelpModal {
    context: CommandHelpContext,
    tab: CommandHelpTab,
    selected: usize,
}

impl CommandHelpModal {
    #[must_use]
    pub const fn new(context: CommandHelpContext) -> Self {
        Self {
            context,
            tab: CommandHelpTab::Available,
            selected: 0,
        }
    }

    #[must_use]
    pub const fn tab(&self) -> CommandHelpTab {
        self.tab
    }

    #[must_use]
    pub const fn context(&self) -> CommandHelpContext {
        self.context
    }

    #[must_use]
    pub const fn selected(&self) -> usize {
        self.selected
    }

    /// Refresh frame-dependent availability without losing the selected tab.
    pub fn set_context(&mut self, context: CommandHelpContext) {
        self.context = context;
        self.clamp_selection();
    }

    /// Cycle between `Available` and `All` and reset the row cursor.
    pub fn next_tab(&mut self) {
        self.tab = self.tab.toggled();
        self.selected = 0;
    }

    /// With two tabs, previous and next have the same wrap-around result.
    pub fn previous_tab(&mut self) {
        self.next_tab();
    }

    pub fn select_next(&mut self) {
        let len = self.entries().len();
        if len > 0 {
            self.selected = (self.selected + 1) % len;
        }
    }

    pub fn select_previous(&mut self) {
        let len = self.entries().len();
        if len > 0 {
            self.selected = (self.selected + len - 1) % len;
        }
    }

    /// Commands visible on the active tab, in registry order.
    #[must_use]
    pub fn entries(&self) -> Vec<CommandHelpEntry> {
        let all = all_entries(self.context);
        match self.tab {
            CommandHelpTab::Available => all
                .into_iter()
                .filter(CommandHelpEntry::is_available)
                .collect(),
            CommandHelpTab::All => all,
        }
    }

    fn clamp_selection(&mut self) {
        self.selected = self.selected.min(self.entries().len().saturating_sub(1));
    }
}

fn all_entries(context: CommandHelpContext) -> Vec<CommandHelpEntry> {
    let workspace = overview::commands().map(|command| CommandHelpEntry {
        scope: CommandScope::Workspace,
        name: command.name,
        description: command.description,
        usage: command.usage,
        available: context.scope == CommandScope::Workspace
            && (command.name != "garden" || context.garden_available),
    });
    let session = closeup::commands().map(|command| CommandHelpEntry {
        scope: CommandScope::Session,
        name: command.name,
        description: command.description,
        usage: command.usage,
        available: context.scope == CommandScope::Session
            && context.session_available
            && command.name != "diff"
            && (command.name != "agent" || context.agent_available),
    });
    workspace.chain(session).collect()
}

fn tab_row(active: CommandHelpTab) -> String {
    let choices = CommandHelpTab::TABS.map(|tab| {
        let label = match tab {
            CommandHelpTab::Available => "Available",
            CommandHelpTab::All => "All",
        };
        (label, Role::Accent)
    });
    modal::choice_buttons(active.index(), &choices)
}

fn entry_row(entry: CommandHelpEntry, selected: bool) -> String {
    let marker = modal::selection_marker(selected);
    let availability = if entry.available {
        Role::Success.style().paint("●")
    } else {
        Style::new().dim().paint("○")
    };
    let scope = Style::new()
        .dim()
        .paint(&format!("{:<10}", entry.scope.label()));
    let name = Role::Accent
        .style()
        .bold()
        .paint(&format!("{:<10}", entry.name));
    let description = Style::new().dim().paint(entry.description);
    modal::content_line(
        &format!("{marker} {availability} {scope} {name}{description}"),
        INNER_WIDTH,
    )
}

fn body(state: &CommandHelpModal, body_height: usize) -> Vec<String> {
    let entries = state.entries();
    let list_height = body_height.saturating_sub(FIXED_ROWS);
    let rows = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| entry_row(*entry, index == state.selected))
        .collect::<Vec<_>>();
    let mut lines = vec![tab_row(state.tab), String::new()];
    if rows.is_empty() {
        lines.push(modal::empty_notice("no commands are available here"));
    } else {
        lines.extend(modal::bounded_list_rows(&rows, state.selected, list_height));
    }
    let usage = entries
        .get(state.selected)
        .map_or("", CommandHelpEntry::usage);
    lines.push(modal::caption(usage));
    lines.push(modal::footer("Tab/←→: tabs  ↑↓: select  ?/Esc: close"));
    lines
}

#[must_use]
pub fn render(raw_height: usize, raw_width: usize, state: &CommandHelpModal) -> Vec<String> {
    let body_height = modal::reserved_body_height(raw_height, raw_width, BODY_HEIGHT);
    modal::render_body(
        raw_height,
        raw_width,
        "Commands",
        INNER_WIDTH,
        BODY_HEIGHT,
        body(state, body_height),
    )
}

#[must_use]
pub fn render_over(
    raw_height: usize,
    raw_width: usize,
    base: &[String],
    state: &CommandHelpModal,
) -> Vec<String> {
    let body_height = modal::reserved_body_height(raw_height, raw_width, BODY_HEIGHT);
    modal::render_body_over(
        raw_height,
        raw_width,
        base,
        "Commands",
        INNER_WIDTH,
        BODY_HEIGHT,
        body(state, body_height),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        CommandHelpContext, CommandHelpEntry, CommandHelpModal, CommandHelpTab, CommandScope,
        render, render_over,
    };
    use crate::presentation::widgets::{display_width, strip_ansi};

    fn context(scope: CommandScope) -> CommandHelpContext {
        CommandHelpContext {
            scope,
            garden_available: true,
            agent_available: true,
            session_available: true,
        }
    }

    fn plain(state: &CommandHelpModal) -> String {
        render(24, 100, state)
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn available_is_the_default_and_follows_the_current_scope() {
        let workspace = CommandHelpModal::new(context(CommandScope::Workspace));
        assert_eq!(workspace.tab(), CommandHelpTab::Available);
        assert_eq!(workspace.entries().len(), 8);
        assert!(
            workspace
                .entries()
                .iter()
                .all(|entry| { entry.scope() == CommandScope::Workspace && entry.is_available() })
        );

        let session = CommandHelpModal::new(context(CommandScope::Session));
        let names = session
            .entries()
            .iter()
            .map(CommandHelpEntry::name)
            .collect::<Vec<_>>();
        assert_eq!(names, ["agent", "close", "env", "terminal"]);
    }

    #[test]
    fn all_contains_both_registries_and_marks_current_availability() {
        let mut modal = CommandHelpModal::new(CommandHelpContext {
            scope: CommandScope::Workspace,
            garden_available: false,
            agent_available: false,
            session_available: false,
        });
        modal.next_tab();
        assert_eq!(modal.tab(), CommandHelpTab::All);
        assert_eq!(modal.entries().len(), 13);
        assert!(
            modal
                .entries()
                .iter()
                .find(|entry| entry.name() == "garden")
                .is_some_and(|entry| !entry.is_available())
        );
        assert!(
            modal
                .entries()
                .iter()
                .find(|entry| { entry.scope() == CommandScope::Session && entry.name() == "env" })
                .is_some_and(|entry| !entry.is_available())
        );
        let rendered = plain(&modal);
        assert!(rendered.contains("Available"));
        assert!(rendered.contains("All"));
        assert!(rendered.contains("workspace"));
        assert!(rendered.contains("session"));
    }

    #[test]
    fn tab_and_selection_navigation_wrap_and_context_clamps() {
        let mut modal = CommandHelpModal::new(context(CommandScope::Workspace));
        modal.select_previous();
        assert_eq!(modal.selected(), 7);
        modal.select_next();
        assert_eq!(modal.selected(), 0);
        modal.previous_tab();
        assert_eq!(modal.tab(), CommandHelpTab::All);
        modal.select_previous();
        assert_eq!(modal.selected(), 12);
        modal.set_context(CommandHelpContext {
            scope: CommandScope::Session,
            garden_available: false,
            agent_available: false,
            session_available: true,
        });
        modal.next_tab();
        assert_eq!(modal.tab(), CommandHelpTab::Available);
        assert_eq!(modal.selected(), 0);
        assert_eq!(modal.entries().len(), 3);
    }

    #[test]
    fn rendering_is_bounded_and_composes_over_a_background() {
        let modal = CommandHelpModal::new(context(CommandScope::Workspace));
        let rendered = render(12, 42, &modal);
        assert_eq!(rendered.len(), 12);
        assert!(
            rendered
                .iter()
                .all(|line| display_width(&strip_ansi(line)) <= 42)
        );

        let base = vec!["background".to_owned(); 24];
        let over = render_over(24, 100, &base, &modal);
        let text = over
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Commands"));
        assert!(text.contains("clean"));
        assert!(text.contains("Esc: close"));
    }
}

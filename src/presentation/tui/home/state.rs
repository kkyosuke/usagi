//! Pure, terminal-independent state for the home (workspace) screen.
//!
//! The home screen is a small command shell laid out in three panes: the
//! worktree list (left), the command log (right), and a command input line
//! (bottom). [`HomeState`] holds all of it — the selectable worktree list, the
//! current mode, the input buffer and its history, and the output log — with no
//! terminal IO, so the navigation, editing, and command logic are all directly
//! testable.

use crate::domain::workspace_state::WorktreeState;

use super::command::{CommandRegistry, Effect};

/// The opened workspace and the selectable list of its worktrees.
#[derive(Debug, Clone)]
pub struct WorktreeList {
    workspace_name: String,
    worktrees: Vec<WorktreeState>,
    selected_index: usize,
}

impl WorktreeList {
    /// Builds a list for the named workspace, with the cursor at the top.
    pub fn new(workspace_name: impl Into<String>, worktrees: Vec<WorktreeState>) -> Self {
        Self {
            workspace_name: workspace_name.into(),
            worktrees,
            selected_index: 0,
        }
    }

    pub fn workspace_name(&self) -> &str {
        &self.workspace_name
    }

    pub fn worktrees(&self) -> &[WorktreeState] {
        &self.worktrees
    }

    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    pub fn is_empty(&self) -> bool {
        self.worktrees.is_empty()
    }

    /// The worktree under the cursor, or `None` when the list is empty.
    pub fn selected(&self) -> Option<&WorktreeState> {
        self.worktrees.get(self.selected_index)
    }

    /// Move the cursor up one row, wrapping to the bottom. No-op when empty.
    pub fn move_up(&mut self) {
        if self.worktrees.is_empty() {
            return;
        }
        self.selected_index = self
            .selected_index
            .checked_sub(1)
            .unwrap_or(self.worktrees.len() - 1);
    }

    /// Move the cursor down one row, wrapping to the top. No-op when empty.
    pub fn move_down(&mut self) {
        if self.worktrees.is_empty() {
            return;
        }
        self.selected_index = (self.selected_index + 1) % self.worktrees.len();
    }
}

/// Which part of the screen currently has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Navigating the worktree list (the default).
    Sidebar,
    /// Typing into the command input line.
    Command,
}

/// The kind of a log line, which decides how it is coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// A command the user entered (echoed back).
    Command,
    /// Ordinary command output.
    Output,
    /// An error (e.g. an unknown command).
    Error,
    /// A transient notice (e.g. a "coming soon" message).
    Notice,
}

/// A single line in the output log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub kind: LineKind,
    pub text: String,
}

impl LogLine {
    pub fn command(text: impl Into<String>) -> Self {
        Self {
            kind: LineKind::Command,
            text: text.into(),
        }
    }

    pub fn output(text: impl Into<String>) -> Self {
        Self {
            kind: LineKind::Output,
            text: text.into(),
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            kind: LineKind::Error,
            text: text.into(),
        }
    }

    pub fn notice(text: impl Into<String>) -> Self {
        Self {
            kind: LineKind::Notice,
            text: text.into(),
        }
    }
}

/// The outcome of submitting the command line: the side effect to act on, plus
/// the command that was recorded in history (so the event loop can persist it).
#[derive(Debug)]
pub struct Submission {
    pub effect: Effect,
    /// The command that was run and added to history, or `None` for empty input.
    pub recorded: Option<String>,
}

/// The full state of the home screen.
///
/// Not `Clone`/`Debug`: it owns a [`CommandRegistry`] of trait objects, which
/// are neither. Nothing needs to clone or format the whole screen state.
pub struct HomeState {
    list: WorktreeList,
    mode: Mode,
    input: String,
    history: Vec<String>,
    /// Index into `history` while recalling past commands; `None` when editing
    /// a fresh line.
    recall: Option<usize>,
    log: Vec<LogLine>,
    /// The commands available in command mode (the extension point for the
    /// follow-up command features).
    registry: CommandRegistry,
}

impl HomeState {
    /// Builds the screen state for `workspace_name` and its `worktrees`. An
    /// optional `notice` (e.g. a load error) seeds the log below a short hint.
    pub fn new(
        workspace_name: impl Into<String>,
        worktrees: Vec<WorktreeState>,
        notice: Option<String>,
    ) -> Self {
        let mut log = vec![LogLine::output(
            "Type \":\" to enter a command, then \"man\" for help.",
        )];
        if let Some(notice) = notice {
            log.push(LogLine::error(notice));
        }
        Self {
            list: WorktreeList::new(workspace_name, worktrees),
            mode: Mode::Sidebar,
            input: String::new(),
            history: Vec::new(),
            recall: None,
            log,
            registry: CommandRegistry::with_builtins(),
        }
    }

    /// Seed the command history with entries restored from disk (oldest first),
    /// so `history` and `↑`/`↓` recall reflect commands run in past sessions.
    pub fn restore_history(&mut self, entries: Vec<String>) {
        self.history = entries;
    }

    pub fn list(&self) -> &WorktreeList {
        &self.list
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn input(&self) -> &str {
        &self.input
    }

    pub fn log(&self) -> &[LogLine] {
        &self.log
    }

    /// Move the worktree cursor up (sidebar mode).
    pub fn move_up(&mut self) {
        self.list.move_up();
    }

    /// Move the worktree cursor down (sidebar mode).
    pub fn move_down(&mut self) {
        self.list.move_down();
    }

    /// Act on the selected worktree. Opening one is not built yet, so this just
    /// logs a "coming soon" notice. No-op when the list is empty.
    pub fn select_worktree(&mut self) {
        if let Some(worktree) = self.list.selected() {
            let branch = worktree.branch.as_deref().unwrap_or("(detached)");
            self.log.push(LogLine::notice(format!(
                "Opening \"{branch}\" is coming soon 🐰"
            )));
        }
    }

    /// Switch from the sidebar to the command input line.
    pub fn enter_command_mode(&mut self) {
        self.mode = Mode::Command;
    }

    /// Leave the command input line, discarding the half-typed input.
    pub fn leave_command_mode(&mut self) {
        self.mode = Mode::Sidebar;
        self.input.clear();
        self.recall = None;
    }

    /// Append a typed character to the input (command mode).
    pub fn push_char(&mut self, c: char) {
        self.input.push(c);
        self.recall = None;
    }

    /// Delete the last character of the input (command mode).
    pub fn backspace(&mut self) {
        self.input.pop();
        self.recall = None;
    }

    /// Tab-complete the command word, listing candidates when ambiguous.
    pub fn complete(&mut self) {
        let completion = self.registry.complete(&self.input);
        self.input = completion.input;
        if !completion.candidates.is_empty() {
            self.log
                .push(LogLine::output(completion.candidates.join("  ")));
        }
        self.recall = None;
    }

    /// Recall the previous (older) command into the input.
    pub fn recall_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let index = match self.recall {
            None => self.history.len() - 1,
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.recall = Some(index);
        self.input = self.history[index].clone();
    }

    /// Recall the next (newer) command, returning to an empty line past the end.
    pub fn recall_next(&mut self) {
        let index = match self.recall {
            None => return,
            Some(i) => i,
        };
        if index + 1 < self.history.len() {
            self.recall = Some(index + 1);
            self.input = self.history[index + 1].clone();
        } else {
            self.recall = None;
            self.input.clear();
        }
    }

    /// Run the current input as a command: echo it, dispatch it, record it in
    /// history, and apply the resulting log lines and side effect. Returns a
    /// [`Submission`] carrying the side effect (so the event loop can act on
    /// `Quit`) and the recorded command (so it can be persisted). Empty input is
    /// a no-op.
    pub fn submit(&mut self) -> Submission {
        let entry = self.input.trim().to_string();
        self.input.clear();
        self.recall = None;
        if entry.is_empty() {
            return Submission {
                effect: Effect::None,
                recorded: None,
            };
        }

        self.log.push(LogLine::command(entry.clone()));
        let result = self.registry.dispatch(&entry, &self.history);
        self.history.push(entry.clone());

        match result.effect {
            Effect::Clear => self.log.clear(),
            _ => self.log.extend(result.lines),
        }
        Submission {
            effect: result.effect,
            recorded: Some(entry),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::workspace_state::BranchStatus;
    use chrono::Utc;
    use std::path::PathBuf;

    fn worktree(branch: &str) -> WorktreeState {
        WorktreeState {
            branch: Some(branch.to_string()),
            path: PathBuf::from(format!("/repo/{branch}")),
            head: "abc1234".to_string(),
            primary: false,
            upstream: None,
            status: BranchStatus::Local,
            updated_at: Utc::now(),
        }
    }

    fn sample() -> WorktreeList {
        WorktreeList::new(
            "usagi",
            vec![worktree("main"), worktree("feature"), worktree("fix")],
        )
    }

    #[test]
    fn new_list_starts_at_the_top() {
        let list = sample();
        assert_eq!(list.workspace_name(), "usagi");
        assert_eq!(list.selected_index(), 0);
        assert_eq!(list.worktrees().len(), 3);
        assert!(!list.is_empty());
        assert_eq!(list.selected().unwrap().branch.as_deref(), Some("main"));
    }

    #[test]
    fn empty_list_has_no_selection() {
        let list = WorktreeList::new("usagi", Vec::new());
        assert!(list.is_empty());
        assert!(list.selected().is_none());
    }

    #[test]
    fn move_down_advances_and_wraps() {
        let mut list = sample();
        list.move_down();
        assert_eq!(list.selected_index(), 1);
        list.move_down();
        list.move_down();
        // Wraps from the last item back to the first.
        assert_eq!(list.selected_index(), 0);
        assert_eq!(list.selected().unwrap().branch.as_deref(), Some("main"));
    }

    #[test]
    fn move_up_wraps_to_the_bottom() {
        let mut list = sample();
        list.move_up();
        assert_eq!(list.selected_index(), 2);
        assert_eq!(list.selected().unwrap().branch.as_deref(), Some("fix"));
        list.move_up();
        assert_eq!(list.selected_index(), 1);
    }

    #[test]
    fn movement_is_a_noop_on_an_empty_list() {
        let mut list = WorktreeList::new("usagi", Vec::new());
        list.move_up();
        assert_eq!(list.selected_index(), 0);
        list.move_down();
        assert_eq!(list.selected_index(), 0);
    }

    // --- HomeState ---------------------------------------------------------

    fn state() -> HomeState {
        HomeState::new("usagi", vec![worktree("main"), worktree("feature")], None)
    }

    #[test]
    fn new_state_starts_in_sidebar_with_a_hint() {
        let state = state();
        assert_eq!(state.mode(), Mode::Sidebar);
        assert_eq!(state.input(), "");
        assert_eq!(state.list().worktrees().len(), 2);
        // The seed log carries the usage hint.
        assert_eq!(state.log().len(), 1);
        assert!(state.log()[0].text.contains("man"));
    }

    #[test]
    fn a_notice_is_seeded_as_an_error_line() {
        let state = HomeState::new("usagi", Vec::new(), Some("load failed".to_string()));
        assert_eq!(state.log().len(), 2);
        assert_eq!(state.log()[1].kind, LineKind::Error);
        assert_eq!(state.log()[1].text, "load failed");
    }

    #[test]
    fn worktree_navigation_delegates_to_the_list() {
        let mut state = state();
        state.move_down();
        assert_eq!(state.list().selected_index(), 1);
        state.move_up();
        assert_eq!(state.list().selected_index(), 0);
    }

    #[test]
    fn selecting_a_worktree_logs_a_coming_soon_notice() {
        let mut state = state();
        state.select_worktree();
        let last = state.log().last().unwrap();
        assert_eq!(last.kind, LineKind::Notice);
        assert!(last.text.contains("main"));
        assert!(last.text.contains("coming soon"));
    }

    #[test]
    fn selecting_on_an_empty_list_does_nothing() {
        let mut state = HomeState::new("usagi", Vec::new(), None);
        let before = state.log().len();
        state.select_worktree();
        assert_eq!(state.log().len(), before);
    }

    #[test]
    fn mode_switching_clears_half_typed_input() {
        let mut state = state();
        state.enter_command_mode();
        assert_eq!(state.mode(), Mode::Command);
        state.push_char('a');
        state.push_char('b');
        assert_eq!(state.input(), "ab");
        state.leave_command_mode();
        assert_eq!(state.mode(), Mode::Sidebar);
        assert_eq!(state.input(), "");
    }

    #[test]
    fn backspace_removes_the_last_character() {
        let mut state = state();
        state.push_char('m');
        state.push_char('a');
        state.backspace();
        assert_eq!(state.input(), "m");
        state.backspace();
        state.backspace(); // popping past empty is harmless
        assert_eq!(state.input(), "");
    }

    #[test]
    fn tab_completes_a_unique_command() {
        let mut state = state();
        state.push_char('d');
        state.push_char('o');
        state.push_char('c');
        state.complete();
        assert_eq!(state.input(), "doctor");
        // A unique completion adds nothing to the log.
        assert_eq!(state.log().len(), 1);
    }

    #[test]
    fn tab_lists_candidates_when_ambiguous() {
        let mut state = state();
        state.push_char('s');
        state.complete();
        assert_eq!(state.input(), "s");
        let last = state.log().last().unwrap();
        assert!(last.text.contains("session"));
        assert!(last.text.contains("space"));
    }

    #[test]
    fn submitting_an_empty_line_is_a_noop() {
        let mut state = state();
        let before = state.log().len();
        let submission = state.submit();
        assert_eq!(submission.effect, Effect::None);
        assert!(submission.recorded.is_none());
        assert_eq!(state.log().len(), before);
    }

    #[test]
    fn submitting_a_command_echoes_and_runs_it() {
        let mut state = state();
        for c in "man".chars() {
            state.push_char(c);
        }
        let submission = state.submit();
        assert_eq!(submission.effect, Effect::None);
        assert_eq!(submission.recorded.as_deref(), Some("man"));
        // The echoed command line is followed by the man output.
        let echoed = state.log().iter().find(|l| l.kind == LineKind::Command);
        assert_eq!(echoed.unwrap().text, "man");
        assert!(state.log().iter().any(|l| l.text.contains("Available")));
        assert_eq!(state.input(), "");
    }

    #[test]
    fn clear_command_empties_the_log() {
        let mut state = state();
        for c in "clear".chars() {
            state.push_char(c);
        }
        assert_eq!(state.submit().effect, Effect::Clear);
        assert!(state.log().is_empty());
    }

    #[test]
    fn quit_command_returns_the_quit_effect() {
        let mut state = state();
        for c in "quit".chars() {
            state.push_char(c);
        }
        assert_eq!(state.submit().effect, Effect::Quit);
    }

    #[test]
    fn submitted_commands_are_recorded_in_history() {
        let mut state = state();
        for c in "man".chars() {
            state.push_char(c);
        }
        state.submit();
        for c in "doctor".chars() {
            state.push_char(c);
        }
        state.submit();
        assert_eq!(state.history, vec!["man", "doctor"]);
    }

    #[test]
    fn restored_history_feeds_recall_and_new_commands_append_to_it() {
        let mut state = state();
        state.restore_history(vec!["session".to_string(), "space".to_string()]);
        // Recall walks the restored entries.
        state.enter_command_mode();
        state.recall_prev();
        assert_eq!(state.input(), "space");
        state.recall_prev();
        assert_eq!(state.input(), "session");
        // A freshly run command is appended after the restored ones.
        state.input = "man".to_string();
        state.submit();
        assert_eq!(state.history, vec!["session", "space", "man"]);
    }

    #[test]
    fn history_recall_walks_backwards_and_forwards() {
        let mut state = state();
        for entry in ["man", "doctor"] {
            for c in entry.chars() {
                state.push_char(c);
            }
            state.submit();
        }

        // Recalling with no prior recall jumps to the most recent entry.
        state.recall_prev();
        assert_eq!(state.input(), "doctor");
        // Older.
        state.recall_prev();
        assert_eq!(state.input(), "man");
        // Already at the oldest — stays put.
        state.recall_prev();
        assert_eq!(state.input(), "man");
        // Forward again towards the newest.
        state.recall_next();
        assert_eq!(state.input(), "doctor");
        // Past the newest returns to an empty line.
        state.recall_next();
        assert_eq!(state.input(), "");
    }

    #[test]
    fn recall_prev_is_a_noop_without_history() {
        let mut state = state();
        state.recall_prev();
        assert_eq!(state.input(), "");
    }

    #[test]
    fn recall_next_without_active_recall_is_a_noop() {
        let mut state = state();
        for c in "man".chars() {
            state.push_char(c);
        }
        state.submit();
        // No recall in progress: recall_next does nothing.
        state.recall_next();
        assert_eq!(state.input(), "");
    }

    #[test]
    fn typing_or_completing_cancels_an_active_recall() {
        let mut state = state();
        for c in "man".chars() {
            state.push_char(c);
        }
        state.submit();
        state.recall_prev();
        assert_eq!(state.input(), "man");
        // Editing resets the recall cursor, so recall_next no longer advances.
        state.push_char('!');
        state.recall_next();
        assert_eq!(state.input(), "man!");
    }
}

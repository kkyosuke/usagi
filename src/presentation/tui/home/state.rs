//! Pure, terminal-independent state for the home (workspace) screen.
//!
//! The home screen is a small command shell laid out in three panes: the
//! worktree list (left), the command log (right), and a command input line
//! (bottom). [`HomeState`] holds all of it — the selectable worktree list, the
//! current mode, the input buffer and its history, and the output log — with no
//! terminal IO, so the navigation, editing, and command logic are all directly
//! testable.

use crate::domain::workspace_state::{SessionRecord, WorktreeState};

use super::command::{CommandRegistry, Effect, WorktreeRef};

/// The display name of a worktree: its branch, or a placeholder when detached.
pub fn worktree_name(worktree: &WorktreeState) -> &str {
    worktree.branch.as_deref().unwrap_or("(detached)")
}

/// The opened workspace and the selectable list of its worktrees.
///
/// Two cursors are tracked: `selected_index` is where the keyboard cursor sits
/// while navigating, and `active_index` is the worktree subsequent commands
/// (`space`, and later `terminal`/`ai`) act on.
#[derive(Debug, Clone)]
pub struct WorktreeList {
    workspace_name: String,
    worktrees: Vec<WorktreeState>,
    selected_index: usize,
    active_index: usize,
}

impl WorktreeList {
    /// Builds a list for the named workspace, with the cursor at the top and
    /// the first worktree (the primary) active.
    pub fn new(workspace_name: impl Into<String>, worktrees: Vec<WorktreeState>) -> Self {
        Self {
            workspace_name: workspace_name.into(),
            worktrees,
            selected_index: 0,
            active_index: 0,
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

    /// Index of the active worktree (the one commands act on).
    pub fn active_index(&self) -> usize {
        self.active_index
    }

    pub fn is_empty(&self) -> bool {
        self.worktrees.is_empty()
    }

    /// The worktree under the cursor, or `None` when the list is empty.
    pub fn selected(&self) -> Option<&WorktreeState> {
        self.worktrees.get(self.selected_index)
    }

    /// The active worktree, or `None` when the list is empty.
    pub fn active(&self) -> Option<&WorktreeState> {
        self.worktrees.get(self.active_index)
    }

    /// Make the worktree under the cursor active, returning its name. No-op
    /// (returning `None`) when the list is empty.
    pub fn activate_selected(&mut self) -> Option<&str> {
        if self.worktrees.is_empty() {
            return None;
        }
        self.active_index = self.selected_index;
        self.active().map(worktree_name)
    }

    /// Make the worktree named `name` active, returning whether one matched.
    pub fn activate_by_name(&mut self, name: &str) -> bool {
        match self.worktrees.iter().position(|w| worktree_name(w) == name) {
            Some(index) => {
                self.active_index = index;
                true
            }
            None => false,
        }
    }

    /// The worktrees as command-facing [`WorktreeRef`]s (name + active flag).
    pub fn refs(&self) -> Vec<WorktreeRef> {
        self.worktrees
            .iter()
            .enumerate()
            .map(|(i, w)| WorktreeRef {
                name: worktree_name(w).to_string(),
                active: i == self.active_index,
            })
            .collect()
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

/// The open session-name modal: the name being typed plus an optional inline
/// validation error (e.g. an empty or duplicate name).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionModal {
    input: String,
    error: Option<String>,
}

impl SessionModal {
    /// The name typed so far.
    pub fn input(&self) -> &str {
        &self.input
    }

    /// The current validation error, if any.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

/// The result of attempting to create a session, applied back to the screen by
/// [`HomeState::apply_session_outcome`]. The impure work (git / filesystem) is
/// done by the event loop's callback; this carries only what the screen shows.
#[derive(Debug, Clone)]
pub struct SessionOutcome {
    /// A line describing the result (success or failure) to append to the log.
    pub line: LogLine,
    /// The refreshed session list, when the action changed it. The worktree
    /// pane is rebuilt from this (each session contributes its worktrees).
    pub sessions: Option<Vec<SessionRecord>>,
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
    /// The session-name modal, when open. While set it captures all keys.
    modal: Option<SessionModal>,
    /// Sessions recorded for this workspace (from `state.json`), shown by
    /// `session list` and kept current as sessions are created.
    sessions: Vec<SessionRecord>,
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
            modal: None,
            sessions: Vec::new(),
        }
    }

    /// Seed the command history with entries restored from disk (oldest first),
    /// so `history` and `↑`/`↓` recall reflect commands run in past sessions.
    pub fn restore_history(&mut self, entries: Vec<String>) {
        self.history = entries;
    }

    /// Seed the recorded sessions (from `state.json`), shown by `session list`,
    /// and rebuild the worktree pane from them.
    pub fn restore_sessions(&mut self, sessions: Vec<SessionRecord>) {
        self.sessions = sessions;
        self.rebuild_list();
    }

    /// Rebuild the worktree pane from the current sessions: every session
    /// contributes its per-repository worktrees, in order.
    fn rebuild_list(&mut self) {
        let name = self.list.workspace_name().to_string();
        let worktrees = self
            .sessions
            .iter()
            .flat_map(|s| s.worktrees.iter().cloned())
            .collect();
        self.list = WorktreeList::new(name, worktrees);
    }

    pub fn sessions(&self) -> &[SessionRecord] {
        &self.sessions
    }

    /// Append the recorded sessions to the log (the `session list` command).
    pub fn log_sessions(&mut self) {
        if self.sessions.is_empty() {
            self.log.push(LogLine::output(
                "No sessions yet. Run \"session new <name>\" to create one.",
            ));
            return;
        }
        self.log.push(LogLine::output(format!(
            "{} session(s):",
            self.sessions.len()
        )));
        for session in &self.sessions {
            self.log.push(LogLine::output(format!(
                "  {}  ({} worktree(s))",
                session.name,
                session.worktrees.len()
            )));
        }
    }

    /// Append an ordinary output line to the log (used by the event loop to
    /// report the result of a command's side effect, e.g. `terminal`).
    pub fn log_output(&mut self, text: impl Into<String>) {
        self.log.push(LogLine::output(text));
    }

    /// Append an error line to the log.
    pub fn log_error(&mut self, text: impl Into<String>) {
        self.log.push(LogLine::error(text));
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

    /// Make the worktree under the cursor the active one (the target of
    /// subsequent commands), logging the switch. No-op when the list is empty.
    pub fn select_worktree(&mut self) {
        if let Some(name) = self.list.activate_selected() {
            let name = name.to_string();
            self.log
                .push(LogLine::notice(format!("Switched to workspace \"{name}\"")));
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
        let result = self
            .registry
            .dispatch(&entry, &self.history, &self.list.refs());
        self.history.push(entry.clone());

        match result.effect {
            Effect::Clear => self.log.clear(),
            // `session switch <name>`: the screen owns the worktree list, so it resolves
            // the name and reports success or failure here.
            Effect::Activate(ref name) => {
                if self.list.activate_by_name(name) {
                    self.log
                        .push(LogLine::notice(format!("Switched to workspace \"{name}\"")));
                } else {
                    self.log
                        .push(LogLine::error(format!("no worktree named \"{name}\"")));
                }
            }
            _ => self.log.extend(result.lines),
        }
        Submission {
            effect: result.effect,
            recorded: Some(entry),
        }
    }

    /// The open session-name modal, if any.
    pub fn modal(&self) -> Option<&SessionModal> {
        self.modal.as_ref()
    }

    /// Open the session-name modal with an empty input.
    pub fn open_session_modal(&mut self) {
        self.modal = Some(SessionModal::default());
    }

    /// Append a typed character to the modal's name (no-op when closed).
    pub fn modal_push_char(&mut self, c: char) {
        if let Some(modal) = self.modal.as_mut() {
            modal.input.push(c);
            modal.error = None;
        }
    }

    /// Delete the last character of the modal's name (no-op when closed).
    pub fn modal_backspace(&mut self) {
        if let Some(modal) = self.modal.as_mut() {
            modal.input.pop();
            modal.error = None;
        }
    }

    /// Close the modal, discarding the half-typed name.
    pub fn cancel_modal(&mut self) {
        self.modal = None;
    }

    /// Validate and accept the modal's name. On success the modal closes and the
    /// trimmed name is returned (for the event loop to create the session); on an
    /// empty or duplicate name the modal stays open with an inline error and
    /// `None` is returned. A no-op (returning `None`) when the modal is closed.
    pub fn submit_modal(&mut self) -> Option<String> {
        let modal = self.modal.as_mut()?;
        let name = modal.input.trim().to_string();
        if name.is_empty() {
            modal.error = Some("Name must not be empty.".to_string());
            return None;
        }
        if self
            .list
            .worktrees()
            .iter()
            .any(|w| w.branch.as_deref() == Some(name.as_str()))
        {
            modal.error = Some(format!("\"{name}\" already exists."));
            return None;
        }
        self.modal = None;
        Some(name)
    }

    /// Apply the result of a session-creation attempt: log its line and, when
    /// creation refreshed the worktree list, swap it in.
    pub fn apply_session_outcome(&mut self, outcome: SessionOutcome) {
        self.log.push(outcome.line);
        if let Some(sessions) = outcome.sessions {
            self.sessions = sessions;
            self.rebuild_list();
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

    #[test]
    fn the_first_worktree_is_active_by_default() {
        let list = sample();
        assert_eq!(list.active_index(), 0);
        assert_eq!(list.active().unwrap().branch.as_deref(), Some("main"));
    }

    #[test]
    fn activate_selected_follows_the_cursor() {
        let mut list = sample();
        list.move_down(); // cursor on "feature"
        assert_eq!(list.activate_selected(), Some("feature"));
        assert_eq!(list.active_index(), 1);
        // The cursor and the active worktree are independent afterwards.
        list.move_down(); // cursor on "fix"
        assert_eq!(list.active_index(), 1);
        assert_eq!(list.selected_index(), 2);
    }

    #[test]
    fn activate_selected_on_an_empty_list_is_a_noop() {
        let mut list = WorktreeList::new("usagi", Vec::new());
        assert_eq!(list.activate_selected(), None);
        assert!(list.active().is_none());
    }

    #[test]
    fn activate_by_name_matches_or_reports_missing() {
        let mut list = sample();
        assert!(list.activate_by_name("fix"));
        assert_eq!(list.active_index(), 2);
        assert!(!list.activate_by_name("nope"));
        // A failed lookup leaves the active worktree unchanged.
        assert_eq!(list.active_index(), 2);
    }

    #[test]
    fn refs_expose_names_and_the_active_flag() {
        let mut list = sample();
        list.activate_by_name("feature");
        let refs = list.refs();
        assert_eq!(refs.len(), 3);
        assert_eq!(refs[0].name, "main");
        assert!(!refs[0].active);
        assert_eq!(refs[1].name, "feature");
        assert!(refs[1].active);
    }

    #[test]
    fn worktree_name_falls_back_to_detached() {
        let mut detached = worktree("main");
        detached.branch = None;
        assert_eq!(worktree_name(&detached), "(detached)");
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
    fn selecting_a_worktree_activates_it() {
        let mut state = state();
        state.move_down(); // cursor on "feature"
        state.select_worktree();
        assert_eq!(state.list().active_index(), 1);
        let last = state.log().last().unwrap();
        assert_eq!(last.kind, LineKind::Notice);
        assert!(last.text.contains("feature"));
        assert!(last.text.contains("Switched"));
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
        // Empty input matches every command, so Tab lists them all as candidates.
        state.complete();
        assert_eq!(state.input(), "");
        let last = state.log().last().unwrap();
        assert!(last.text.contains("session"));
        assert!(last.text.contains("man"));
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
    fn session_switch_changes_the_active_worktree() {
        let mut state = state(); // main (active), feature
        for c in "session switch feature".chars() {
            state.push_char(c);
        }
        let submission = state.submit();
        assert!(matches!(submission.effect, Effect::Activate(_)));
        assert_eq!(state.list().active_index(), 1);
        let last = state.log().last().unwrap();
        assert_eq!(last.kind, LineKind::Notice);
        assert!(last.text.contains("feature"));
    }

    #[test]
    fn session_switch_with_an_unknown_name_errors() {
        let mut state = state();
        for c in "session switch nope".chars() {
            state.push_char(c);
        }
        state.submit();
        // The active worktree is unchanged and an error is logged.
        assert_eq!(state.list().active_index(), 0);
        let last = state.log().last().unwrap();
        assert_eq!(last.kind, LineKind::Error);
        assert!(last.text.contains("no worktree named"));
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
    fn session_command_without_a_name_opens_the_modal() {
        let mut state = state();
        for c in "session new".chars() {
            state.push_char(c);
        }
        let submission = state.submit();
        assert_eq!(submission.effect, Effect::OpenSessionModal);
        // The event loop turns the effect into an open modal.
        assert!(state.modal().is_none());
        state.open_session_modal();
        assert_eq!(state.modal().unwrap().input(), "");
        assert!(state.modal().unwrap().error().is_none());
    }

    #[test]
    fn modal_editing_appends_and_deletes_characters() {
        let mut state = state();
        state.open_session_modal();
        state.modal_push_char('a');
        state.modal_push_char('b');
        assert_eq!(state.modal().unwrap().input(), "ab");
        state.modal_backspace();
        assert_eq!(state.modal().unwrap().input(), "a");
    }

    #[test]
    fn modal_editing_is_a_noop_when_closed() {
        let mut state = state();
        // No modal open: editing keys are harmless.
        state.modal_push_char('a');
        state.modal_backspace();
        assert!(state.modal().is_none());
        assert!(state.submit_modal().is_none());
    }

    #[test]
    fn cancel_modal_closes_it() {
        let mut state = state();
        state.open_session_modal();
        state.modal_push_char('x');
        state.cancel_modal();
        assert!(state.modal().is_none());
    }

    #[test]
    fn submit_modal_rejects_an_empty_name() {
        let mut state = state();
        state.open_session_modal();
        // Whitespace only is empty after trimming.
        state.modal_push_char(' ');
        assert!(state.submit_modal().is_none());
        let modal = state.modal().unwrap();
        assert!(modal.error().unwrap().contains("must not be empty"));
    }

    #[test]
    fn submit_modal_rejects_a_duplicate_name() {
        // The sample state has a "feature" worktree; reusing it is rejected.
        let mut state = state();
        state.open_session_modal();
        for c in "feature".chars() {
            state.modal_push_char(c);
        }
        assert!(state.submit_modal().is_none());
        assert!(state.modal().unwrap().error().unwrap().contains("feature"));
    }

    #[test]
    fn submit_modal_accepts_a_fresh_name_and_closes() {
        let mut state = state();
        state.open_session_modal();
        for c in "  fresh  ".chars() {
            state.modal_push_char(c);
        }
        // The name is trimmed, the modal closes, and the name is returned.
        assert_eq!(state.submit_modal().as_deref(), Some("fresh"));
        assert!(state.modal().is_none());
    }

    fn session_record(name: &str, worktrees: usize) -> SessionRecord {
        SessionRecord {
            name: name.to_string(),
            root: std::path::PathBuf::from(format!("/repo/.usagi/worktree/{name}")),
            // Each repository's worktree is on the session branch `name`.
            worktrees: (0..worktrees).map(|_| worktree(name)).collect(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn apply_session_outcome_logs_and_rebuilds_the_pane_from_sessions() {
        let mut state = state();
        // A success outcome with refreshed sessions rebuilds the worktree pane
        // from them (one worktree per session here).
        state.apply_session_outcome(SessionOutcome {
            line: LogLine::output("Created session \"x\""),
            sessions: Some(vec![session_record("main", 1), session_record("x", 1)]),
        });
        assert!(state.log().last().unwrap().text.contains("Created session"));
        assert_eq!(state.sessions().len(), 2);
        assert_eq!(state.list().worktrees().len(), 2);
        assert_eq!(state.list().workspace_name(), "usagi");
        assert!(state
            .list()
            .worktrees()
            .iter()
            .any(|w| w.branch.as_deref() == Some("x")));

        // A failure outcome (no sessions) only logs; the pane is unchanged.
        state.apply_session_outcome(SessionOutcome {
            line: LogLine::error("session failed"),
            sessions: None,
        });
        assert_eq!(state.log().last().unwrap().kind, LineKind::Error);
        assert_eq!(state.list().worktrees().len(), 2);
        assert_eq!(state.sessions().len(), 2);
    }

    #[test]
    fn log_sessions_lists_recorded_sessions() {
        let mut state = state();
        state.restore_sessions(vec![session_record("alpha", 2), session_record("beta", 1)]);
        state.log_sessions();
        let text = state
            .log()
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("2 session(s)"));
        assert!(text.contains("alpha"));
        assert!(text.contains("beta"));
    }

    #[test]
    fn log_sessions_reports_when_empty() {
        let mut state = state();
        state.log_sessions();
        assert!(state.log().last().unwrap().text.contains("No sessions yet"));
    }

    #[test]
    fn log_output_and_error_append_lines() {
        let mut state = state();
        state.log_output("did a thing");
        state.log_error("it broke");
        let last_two: Vec<_> = state.log().iter().rev().take(2).collect();
        assert_eq!(last_two[0].kind, LineKind::Error);
        assert_eq!(last_two[0].text, "it broke");
        assert_eq!(last_two[1].kind, LineKind::Output);
        assert_eq!(last_two[1].text, "did a thing");
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

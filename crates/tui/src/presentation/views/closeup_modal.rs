//! Closeup modal（セッションのアクションメニュー）。
//!
//! workspace 画面でフォーカス中のセッションに対する操作を選ぶ小さな中央メニュー。↑↓ で選ぶ。
//! 中央に浮かぶ枠付きダイアログとして描く（枠・配置は共通の [`modal`]
//! widget に委譲）。
//!
//! 状態 [`CloseupModal`] は端末 IO を持たない純粋な値で、[`render`] が 1 フレーム分の行
//! （ANSI 付き `Vec<String>`）に変換する。キー入力の解釈は入力層が整うときに載せ、ここでは
//! カーソル移動と選択の純粋操作だけを公開する。

use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::{TextInput, modal};
use crate::usecase::{agent_command, closeup};
use usagi_core::domain::settings::{AvailableModels, DefaultModel, ModalSelectionMode};

/// モーダルの枠の内側（内容）幅。
const INNER_WIDTH: usize = 50;
const BODY_HEIGHT: usize = 10;

/// アクションメニューの状態。対象セッション名と、アクション一覧上のカーソルを持つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseupModal {
    session: String,
    selected: usize,
    selection_mode: ModalSelectionMode,
    input: TextInput,
    expanded: bool,
    selected_subcommand: usize,
    /// Agent CLIs installed on this machine. The `agent` picker and its Tab
    /// completion offer only these, so a selection is always runnable.
    available_models: AvailableModels,
    /// The configured provider an `agent` without `-m` launches.
    default_model: DefaultModel,
    /// Where repeated Tab presses are in the candidate list, if a cycle is open.
    completion_cycle: Option<CompletionCycle>,
    /// The reducer's safe message for the last refused submission. A refusal
    /// keeps this modal open, so this line is the only signal the user gets.
    error: Option<String>,
}

/// One open Tab-completion cycle.
///
/// `origin` is the text the cycle started from, kept because each Tab replaces
/// the whole input and would otherwise lose the prefix the candidates came from.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CompletionCycle {
    origin: String,
    index: usize,
}

impl CloseupModal {
    /// セッション `session` を対象に、先頭アクションを選んだメニューを開く。
    #[must_use]
    pub fn new(session: impl Into<String>) -> Self {
        Self::with_selection_mode(session, ModalSelectionMode::Action)
    }

    /// Open a modal using the configured command-selection interaction.
    #[must_use]
    pub fn with_selection_mode(
        session: impl Into<String>,
        selection_mode: ModalSelectionMode,
    ) -> Self {
        Self {
            session: session.into(),
            selected: 0,
            selection_mode,
            input: TextInput::default(),
            expanded: false,
            selected_subcommand: 0,
            available_models: AvailableModels::all(),
            default_model: DefaultModel::default(),
            completion_cycle: None,
            error: None,
        }
    }

    /// Constrain the `agent -m` picker and completion to the installed CLIs and
    /// mark the configured default.
    #[must_use]
    pub fn with_agent_models(mut self, available: AvailableModels, default: DefaultModel) -> Self {
        self.available_models = available;
        self.default_model = default;
        self.selected_subcommand = self
            .selected_subcommand
            .min(self.subcommands().len().saturating_sub(1));
        self
    }

    /// The prompt text currently owned by this modal.
    #[must_use]
    pub fn input(&self) -> &str {
        self.input.value()
    }

    /// The prompt caret's byte offset.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.input.cursor()
    }

    /// 対象セッション名。
    #[must_use]
    pub fn session(&self) -> &str {
        &self.session
    }

    /// Retitle the modal for the active target without disturbing its input
    /// state. The runtime persists one modal across frames but does not track the
    /// session label, so the renderer stamps the current label here.
    #[must_use]
    pub fn with_session(mut self, session: impl Into<String>) -> Self {
        self.session = session.into();
        self
    }

    /// 選択中アクションの添字。
    #[must_use]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Returns whether this modal accepts an action choice or a typed prompt.
    #[must_use]
    pub fn selection_mode(&self) -> ModalSelectionMode {
        self.selection_mode
    }

    /// アクション一覧。
    #[must_use]
    pub fn actions(&self) -> Vec<closeup::CommandInfo> {
        closeup::commands().collect()
    }

    /// 選択中のアクション。
    #[must_use]
    pub fn selected_action(&self) -> closeup::CommandInfo {
        self.matches()[self.selected]
    }

    /// Enter で controller へ渡す registry command。Closeup は入力欄を持たないため、
    /// 選択行の command 名そのものが completion になる。
    #[must_use]
    pub fn submission(&self) -> String {
        match self.selection_mode {
            ModalSelectionMode::Action if self.expanded => self
                .subcommands()
                .get(self.selected_subcommand)
                .map_or_else(String::new, |subcommand| {
                    format!("{} {}", self.selected_action().name, subcommand.value)
                }),
            // Action mode's input starts as a command-name filter, but once the
            // user types an argument separator it is a complete command line.
            // Returning the filtered row here made `agent -m codex` produce an
            // empty submission because no command name starts with that whole
            // string.
            ModalSelectionMode::Action if self.input.value().contains(char::is_whitespace) => {
                self.input.value().to_owned()
            }
            ModalSelectionMode::Action => self
                .matches()
                .get(self.selected)
                .map_or_else(String::new, |action| action.name.to_owned()),
            ModalSelectionMode::Prompt => self.input.value().to_owned(),
        }
    }

    /// Insert one character in Prompt mode.
    pub fn insert_char(&mut self, c: char) {
        self.input.insert(c);
        self.edited();
    }

    /// Insert one bracketed-paste payload at the current caret, replacing any
    /// active selection just like ordinary typing.
    pub fn paste(&mut self, text: &str) {
        self.input.insert_str(text);
        self.edited();
    }

    /// Delete one character in Prompt mode.
    pub fn backspace(&mut self) {
        self.input.backspace();
        self.edited();
    }

    /// Forward-delete one character at the prompt caret in Prompt mode.
    pub fn delete_forward(&mut self) {
        self.input.delete_forward();
        self.edited();
    }

    /// Reset everything derived from the input text after an edit: the filter
    /// selection, the expanded picker, the open Tab cycle (its candidates came
    /// from text that no longer exists), and the previous refusal message.
    fn edited(&mut self) {
        self.selected = 0;
        self.expanded = false;
        self.completion_cycle = None;
        self.error = None;
    }

    /// Show the reducer's safe message for a refused submission, or clear it.
    pub fn set_error(&mut self, message: Option<String>) {
        self.error = message;
    }

    /// The refusal message currently shown, if any.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Move the prompt caret left in Prompt mode. Clears any selection.
    pub fn cursor_left(&mut self) {
        self.input.move_left();
    }

    /// Move the prompt caret right in Prompt mode. Clears any selection.
    pub fn cursor_right(&mut self) {
        self.input.move_right();
    }

    /// Move the prompt caret to the start of the line (Home / Ctrl-A).
    pub fn cursor_home(&mut self) {
        self.input.move_home();
    }

    /// Move the prompt caret to the end of the line (End / Ctrl-E).
    pub fn cursor_end(&mut self) {
        self.input.move_end();
    }

    /// Extend the selection one character left (Shift+Left).
    pub fn select_left(&mut self) {
        self.input.select_left();
    }

    /// Extend the selection one character right (Shift+Right).
    pub fn select_right(&mut self) {
        self.input.select_right();
    }

    /// Extend the selection to the start of the line (Shift+Home).
    pub fn select_home(&mut self) {
        self.input.select_home();
    }

    /// Extend the selection to the end of the line (Shift+End).
    pub fn select_end(&mut self) {
        self.input.select_end();
    }

    /// The prompt input's selection range, if any. Used by the renderer.
    #[must_use]
    pub fn selection(&self) -> Option<(usize, usize)> {
        self.input.selection()
    }

    /// Complete the input from the candidates for what is typed, cycling through
    /// them on repeated presses (`agent -m c` → `claude` → `codex` → `claude`).
    ///
    /// Ambiguity is not a dead end: the first Tab inserts the first candidate and
    /// each further Tab replaces it with the next, so a shared prefix never
    /// leaves the key doing nothing. The cycle is measured from the text it
    /// started at, so it survives its own replacements and ends as soon as the
    /// input is edited. Inputs outside the completion grammar are left untouched.
    pub fn complete_selected(&mut self) {
        let cycle = self.completion_cycle.take();
        let origin = cycle.as_ref().map_or_else(
            || self.input.value().to_owned(),
            |cycle| cycle.origin.clone(),
        );
        let candidates = self.completion_candidates(&origin);
        if candidates.is_empty() {
            return;
        }
        let index = match &cycle {
            Some(cycle) => (cycle.index + 1) % candidates.len(),
            // A fresh command-name cycle starts at the selected row, so ↑↓ then
            // Tab still completes the command the user moved to.
            None if origin.contains(char::is_whitespace) => 0,
            None => self.selected.min(candidates.len() - 1),
        };
        self.input = TextInput::with_value(candidates[index].clone());
        self.selected = 0;
        self.expanded = false;
        self.error = None;
        self.completion_cycle = Some(CompletionCycle { origin, index });
    }

    /// 選択を次へ（末尾で先頭へ回り込む）。
    pub fn select_next(&mut self) {
        self.completion_cycle = None;
        if self.expanded {
            let len = self.subcommands().len();
            if len > 0 {
                self.selected_subcommand = (self.selected_subcommand + 1) % len;
            }
            return;
        }
        let len = self.matches().len();
        if len > 0 {
            self.selected = (self.selected + 1) % len;
        }
    }

    /// 選択を前へ（先頭で末尾へ回り込む）。
    pub fn select_prev(&mut self) {
        self.completion_cycle = None;
        if self.expanded {
            let len = self.subcommands().len();
            if len > 0 {
                self.selected_subcommand = (self.selected_subcommand + len - 1) % len;
            }
            return;
        }
        let len = self.matches().len();
        if len > 0 {
            self.selected = (self.selected + len - 1) % len;
        }
    }

    /// Expand the selected action's inline subcommand picker when available.
    pub fn expand_selected(&mut self) {
        self.completion_cycle = None;
        if !self.matches().is_empty() && !self.subcommands().is_empty() {
            self.expanded = true;
            self.selected_subcommand = 0;
        }
    }

    /// Collapse an inline subcommand picker. Returns whether it was open.
    pub fn collapse(&mut self) -> bool {
        self.completion_cycle = None;
        std::mem::take(&mut self.expanded)
    }

    fn subcommands(&self) -> Vec<ModalSubcommand> {
        let Some(action) = self.matches().get(self.selected).copied() else {
            return Vec::new();
        };
        match action.name {
            "agent" => agent_command::model_choices(self.available_models, self.default_model)
                .into_iter()
                .map(|choice| ModalSubcommand {
                    label: choice.label,
                    value: choice.value,
                })
                .collect(),
            "close" => vec![ModalSubcommand::plain("--force")],
            "terminal" => vec![
                ModalSubcommand::plain("open"),
                ModalSubcommand::plain("new"),
            ],
            _ => Vec::new(),
        }
    }

    /// Every completion for `origin`, each one the full input text that replaces
    /// it. Returning the whole list (not just an unambiguous single hit) is what
    /// lets [`Self::complete_selected`] cycle.
    fn completion_candidates(&self, origin: &str) -> Vec<String> {
        // The command name itself is complete only once a separator follows it;
        // before that, command-name completion owns the input.
        if !origin.contains(char::is_whitespace) {
            return self
                .matches_for(origin)
                .into_iter()
                .map(|action| action.name.to_owned())
                .collect();
        }
        let input = origin.trim_start();
        let Some(separator) = input.find(char::is_whitespace) else {
            return Vec::new();
        };
        let (command, arguments) = input.split_at(separator);
        // `agent` owns a multi-token grammar (`-m <cli>`), so it completes from
        // its own vocabulary rather than this single-token rule.
        if command == "agent" {
            return agent_command::completions(arguments, self.available_models)
                .into_iter()
                .map(|candidate| format!("{command} {candidate}"))
                .collect();
        }
        if input.ends_with(char::is_whitespace) {
            return Vec::new();
        }
        // The check above guarantees a trailing token here, so the default is
        // unreachable rather than a silent match-everything.
        let mut tokens = arguments.split_whitespace();
        let prefix = tokens.next().unwrap_or_default();
        if tokens.next().is_some() {
            return Vec::new();
        }
        let candidates = match command {
            "close" => vec!["--force".to_owned()],
            "terminal" => vec!["open".to_owned(), "new".to_owned()],
            _ => return Vec::new(),
        };
        candidates
            .into_iter()
            .filter(|candidate| candidate.starts_with(prefix))
            .map(|candidate| format!("{command} {candidate}"))
            .collect()
    }

    fn matches(&self) -> Vec<closeup::CommandInfo> {
        self.matches_for(self.input.value())
    }

    fn matches_for(&self, prefix: &str) -> Vec<closeup::CommandInfo> {
        self.actions()
            .into_iter()
            .filter(|action| action.name.starts_with(prefix))
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ModalSubcommand {
    label: String,
    value: String,
}

impl ModalSubcommand {
    fn plain(value: &str) -> Self {
        Self {
            label: value.to_owned(),
            value: value.to_owned(),
        }
    }
}

/// 1 アクション行: 選択中は `›` マーカー、command 名（accent）、説明（dim）。
fn action_row(action: closeup::CommandInfo, selected: bool, inner: usize) -> String {
    let marker = modal::selection_marker(selected);
    let label = Role::Accent
        .style()
        .bold()
        .paint(&format!("{:<14}", action.name));
    let desc = Style::new().dim().paint(action.description);
    modal::content_line(&format!("{marker} {label}{desc}"), inner)
}

/// アクションメニューのボディ（枠の内側の行）。対象セッションは v1 と同様に title にのみ載せる。
fn body(state: &CloseupModal) -> Vec<String> {
    if state.selection_mode == ModalSelectionMode::Prompt {
        return modal::fixed_body(
            vec![
                Style::new().dim().paint("Type a command:"),
                String::new(),
                modal::prompt_line(
                    state.input.value(),
                    state.input.cursor(),
                    state.input.selection(),
                ),
                // A refusal takes the spacer under the prompt: the modal stays
                // open, so this row is the only signal the command was rejected.
                // The box height is unchanged either way.
                error_row(state),
                modal::footer("Tab: complete   Enter: run   Esc: back"),
            ],
            BODY_HEIGHT,
        );
    }
    let mut lines = vec![
        Style::new().dim().paint("Run a command:  (type to filter)"),
        modal::filter_line(
            state.input.value(),
            state.input.cursor(),
            state.input.selection(),
        ),
    ];
    // Above the action list, not next to the footer: an expanded picker pushes
    // the last rows out of the fixed body, and a refusal must stay readable.
    if state.error.is_some() {
        lines.push(error_row(state));
    }
    for (i, action) in state.matches().iter().enumerate() {
        lines.push(action_row(*action, i == state.selected, INNER_WIDTH));
        if state.expanded && i == state.selected {
            for (sub_index, subcommand) in state.subcommands().iter().enumerate() {
                lines.push(modal::subcommand_row(
                    &subcommand.label,
                    sub_index == state.selected_subcommand,
                ));
            }
        }
    }
    if state.error.is_none() {
        lines.push(String::new());
    }
    lines.push(modal::footer(
        "↑↓: select   →: expand   Enter: run   Esc: back",
    ));
    modal::fixed_body(lines, BODY_HEIGHT)
}

/// The refusal row, or an empty spacer when the last submission was not refused.
fn error_row(state: &CloseupModal) -> String {
    state.error.as_deref().map_or_else(String::new, |message| {
        modal::error_line(message, INNER_WIDTH)
    })
}

/// 生の端末サイズに対する closeup modal 1 フレーム分の行。中央に浮かぶ枠付きダイアログとして
/// 描く（枠と中央寄せは [`modal::render_modal`] に委譲）。サイズ 0 は 80×24 にフォールバック。
#[must_use]
pub fn render(raw_height: usize, raw_width: usize, state: &CloseupModal) -> Vec<String> {
    modal::render_modal(
        raw_height,
        raw_width,
        &format!("Closeup: {}", state.session()),
        INNER_WIDTH,
        &body(state),
    )
}

/// `base` の workspace フレームを背景に残し、closeup modal を中央に合成する。
/// 小端末では [`modal::render_body_over`] が背景の帯を残す。サイズ 0 は 80×24 にフォールバックする。
#[must_use]
pub fn render_over(
    raw_height: usize,
    raw_width: usize,
    base: &[String],
    state: &CloseupModal,
) -> Vec<String> {
    modal::render_body_over(
        raw_height,
        raw_width,
        base,
        &format!("Closeup: {}", state.session()),
        INNER_WIDTH,
        BODY_HEIGHT,
        body(state),
    )
}

#[cfg(test)]
mod tests {
    use super::{CloseupModal, render, render_over};
    use crate::presentation::widgets::{display_width, strip_ansi};
    use usagi_core::domain::settings::{AvailableModels, DefaultModel, ModalSelectionMode};

    #[test]
    fn action_selection_keeps_the_closeup_box_height_stable() {
        let mut modal = CloseupModal::new("daemon");
        let before = render(40, 80, &modal)
            .iter()
            .filter(|line| line.contains('│') || line.contains('┌') || line.contains('└'))
            .count();
        modal.select_next();
        let after = render(40, 80, &modal)
            .iter()
            .filter(|line| line.contains('│') || line.contains('┌') || line.contains('└'))
            .count();
        assert_eq!(before, after);
    }

    fn joined(state: &CloseupModal) -> String {
        render(24, 80, state)
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn expanding_an_action_with_subcommands_lists_them() {
        // `terminal` and `close` carry subcommands; expanding the selected action
        // renders that subcommand list (the completion the Ctrl-O command input
        // drives).
        for (action, subcommand) in [("terminal", "open"), ("close", "--force")] {
            let mut modal = CloseupModal::new("daemon");
            for _ in 0..modal.actions().len() {
                if modal.selected_action().name == action {
                    break;
                }
                modal.select_next();
            }
            assert_eq!(modal.selected_action().name, action);
            modal.expand_selected();
            assert!(joined(&modal).contains(subcommand));
        }

        // `agent` expands into its installed `-m` choices, not `close`'s flags.
        let mut agent = CloseupModal::new("daemon");
        assert_eq!(agent.selected_action().name, "agent");
        agent.expand_selected();
        assert!(!joined(&agent).contains("--force"));
        assert!(joined(&agent).contains("-m codex"));
    }

    #[test]
    fn agent_expands_only_installed_clis_and_marks_the_configured_default() {
        let mut modal = CloseupModal::new("daemon").with_agent_models(
            AvailableModels::new([DefaultModel::OpenAi, DefaultModel::SakanaAi]),
            DefaultModel::SakanaAi,
        );
        assert_eq!(modal.selected_action().name, "agent");
        modal.expand_selected();
        let frame = joined(&modal);
        assert!(frame.contains("-m codex"));
        assert!(frame.contains("-m sakana.ai  (default)"));
        // An absent CLI is never offered.
        assert!(!frame.contains("-m claude"));

        // Confirming a row submits the selection as `agent` arguments.
        assert_eq!(modal.submission(), "agent -m codex");
        modal.select_next();
        assert_eq!(modal.submission(), "agent -m sakana.ai");

        // With no CLI installed the action carries no choices and cannot expand.
        let mut none = CloseupModal::new("daemon")
            .with_agent_models(AvailableModels::default(), DefaultModel::OpenAi);
        none.expand_selected();
        assert_eq!(none.submission(), "agent");
        assert!(!joined(&none).contains("-m"));
    }

    #[test]
    fn action_mode_submits_a_typed_command_line_with_arguments() {
        let mut modal = CloseupModal::new("daemon");
        for character in "agent -m codex".chars() {
            modal.insert_char(character);
        }

        assert!(modal.matches().is_empty());
        assert_eq!(modal.submission(), "agent -m codex");

        // A trailing separator is also meaningful: it submits the command and
        // lets the command parser decide whether an omitted argument is valid.
        modal = CloseupModal::new("daemon");
        for character in "agent ".chars() {
            modal.insert_char(character);
        }
        assert_eq!(modal.submission(), "agent ");
    }

    #[test]
    fn an_action_without_subcommands_neither_expands_nor_completes_arguments() {
        // `diff` and Closeup's workspace-only `env` take no arguments, so they
        // have no picker rows.
        for action in ["diff", "env"] {
            let mut modal = CloseupModal::new("s");
            while modal.selected_action().name != action {
                modal.select_next();
            }
            modal.expand_selected();
            assert_eq!(modal.submission(), action);
            assert!(!joined(&modal).contains("-m"));
        }

        // Argument text for either command is outside every completion vocabulary.
        let mut prompt = CloseupModal::with_selection_mode("s", ModalSelectionMode::Prompt);
        for character in "diff s".chars() {
            prompt.insert_char(character);
        }
        prompt.complete_selected();
        assert_eq!(prompt.submission(), "diff s");
    }

    #[test]
    fn tab_completes_the_agent_model_flag_and_only_installed_clis() {
        let models = AvailableModels::new([DefaultModel::Claude, DefaultModel::SakanaAi]);
        let complete = |input: &str| {
            let mut modal = CloseupModal::with_selection_mode("s", ModalSelectionMode::Prompt)
                .with_agent_models(models, DefaultModel::Claude);
            for character in input.chars() {
                modal.insert_char(character);
            }
            modal.complete_selected();
            modal.submission()
        };

        // A unique CLI prefix completes to its full selector.
        assert_eq!(complete("agent -m sak"), "agent -m sakana.ai");
        assert_eq!(complete("agent -m c"), "agent -m claude");
        assert_eq!(complete("agent --model sak"), "agent --model sakana.ai");
        // The flag itself completes from its unique prefix.
        assert_eq!(complete("agent --"), "agent --model");
        // An absent CLI has nothing to complete to, and neither has an unknown
        // prefix or an already complete selection.
        assert_eq!(complete("agent -m cod"), "agent -m cod");
        assert_eq!(complete("agent -m zzz"), "agent -m zzz");
        assert_eq!(complete("agent -m claude"), "agent -m claude");
        // An ambiguous prefix takes the first candidate rather than doing nothing.
        assert_eq!(complete("agent -m "), "agent -m claude");
        // Other commands keep their own single-token completion.
        assert_eq!(complete("terminal n"), "terminal new");
        assert_eq!(complete("close --f"), "close --force");
        assert_eq!(complete("env g"), "env g");
        // Whitespace with no argument position after the command name has
        // nothing to complete; the input is left exactly as typed.
        assert_eq!(complete(" agent"), " agent");
        assert_eq!(complete("terminal open "), "terminal open ");
    }

    #[test]
    fn repeated_tab_cycles_the_candidates_for_what_was_typed() {
        // `c` matches both `claude` and `codex`: the first Tab takes the first
        // candidate and each further Tab advances, wrapping at the end. Without
        // this, a shared prefix left Tab doing nothing at all.
        let mut modal = CloseupModal::with_selection_mode("s", ModalSelectionMode::Prompt)
            .with_agent_models(AvailableModels::all(), DefaultModel::Claude);
        for character in "agent -m c".chars() {
            modal.insert_char(character);
        }
        for expected in [
            "agent -m claude",
            "agent -m codex",
            "agent -m claude",
            "agent -m codex",
        ] {
            modal.complete_selected();
            assert_eq!(modal.submission(), expected);
        }

        // Editing ends the cycle: the next Tab completes from the new text.
        modal.backspace();
        assert_eq!(modal.submission(), "agent -m code");
        modal.complete_selected();
        assert_eq!(modal.submission(), "agent -m codex");

        // The cycle covers the whole vocabulary offered at that position.
        let mut all = CloseupModal::with_selection_mode("s", ModalSelectionMode::Prompt)
            .with_agent_models(AvailableModels::all(), DefaultModel::Claude);
        for character in "agent ".chars() {
            all.insert_char(character);
        }
        let mut seen = Vec::new();
        for _ in 0..5 {
            all.complete_selected();
            seen.push(all.submission());
        }
        assert_eq!(
            seen,
            [
                "agent -m",
                "agent --model",
                "agent claude",
                "agent codex",
                "agent sakana.ai",
            ]
        );
        // Wrapping returns to the first candidate.
        all.complete_selected();
        assert_eq!(all.submission(), "agent -m");
    }

    #[test]
    fn a_command_name_cycle_starts_at_the_selected_row_and_navigation_ends_it() {
        // Command-name completion still honours ↑↓: Tab completes the row the
        // user moved to, then advances from there.
        let mut modal = CloseupModal::new("s");
        modal.select_next(); // close
        modal.complete_selected();
        assert_eq!(modal.submission(), "close");
        modal.complete_selected();
        assert_eq!(modal.submission(), "diff");

        // Moving the selection, expanding, or collapsing ends the open cycle, so
        // the next Tab starts over at the first candidate instead of advancing.
        for interrupt in [
            CloseupModal::select_next,
            CloseupModal::select_prev,
            CloseupModal::expand_selected,
            |modal: &mut CloseupModal| {
                modal.collapse();
            },
        ] {
            let mut restarted = CloseupModal::with_selection_mode("s", ModalSelectionMode::Prompt)
                .with_agent_models(AvailableModels::all(), DefaultModel::Claude);
            for character in "agent -m c".chars() {
                restarted.insert_char(character);
            }
            restarted.complete_selected();
            assert_eq!(restarted.submission(), "agent -m claude");
            interrupt(&mut restarted);
            restarted.complete_selected();
            assert_eq!(restarted.submission(), "agent -m claude");
        }
    }

    #[test]
    fn a_refusal_row_is_drawn_in_danger_without_changing_the_box_height() {
        // The refusal is the only feedback a rejected command gets, so it must be
        // on screen in both interaction modes — and it must not resize the box or
        // be pushed out of the fixed body by an expanded picker.
        for mode in [ModalSelectionMode::Action, ModalSelectionMode::Prompt] {
            let mut modal = CloseupModal::with_selection_mode("s", mode);
            let before = render(24, 80, &modal).len();
            assert!(!joined(&modal).contains("not installed"));

            modal.set_error(Some("that agent CLI is not installed".to_owned()));
            let rendered = render(24, 80, &modal);
            assert_eq!(rendered.len(), before, "{mode:?}");
            assert!(joined(&modal).contains("that agent CLI is not installed"));
            // Danger, so a refusal reads as one at a glance.
            assert!(
                rendered
                    .iter()
                    .any(|line| line.contains("\u{1b}[31m") && line.contains("not installed")),
                "{mode:?}"
            );
            assert_eq!(modal.error(), Some("that agent CLI is not installed"));

            // Expanding the picker keeps the refusal visible.
            modal.expand_selected();
            assert!(joined(&modal).contains("that agent CLI is not installed"));

            // Editing clears it and restores the original body.
            modal.insert_char('a');
            modal.backspace();
            assert_eq!(modal.error(), None);
            assert!(!joined(&modal).contains("not installed"));
        }
    }

    #[test]
    fn a_long_refusal_is_clipped_to_the_modal_body() {
        // A safe message can be longer than the box; clipping keeps the frame
        // rectangular instead of spilling past the border.
        let mut modal = CloseupModal::with_selection_mode("s", ModalSelectionMode::Prompt);
        modal.set_error(Some("x".repeat(super::INNER_WIDTH * 2)));
        for line in render(24, 80, &modal) {
            assert!(display_width(&strip_ansi(&line)) <= 80);
        }
    }

    #[test]
    fn new_modal_targets_the_session_and_lists_actions() {
        let modal = CloseupModal::new("tui");
        assert_eq!(modal.session(), "tui");
        assert_eq!(modal.selected(), 0);
        assert_eq!(modal.actions().len(), 5);
        assert_eq!(modal.selected_action().name, "agent");
        assert!(joined(&modal).contains("env"));
        assert!(joined(&modal).contains("↑↓: select"));
        // derive された Clone / Debug も触れる。
        assert!(format!("{:?}", modal.clone()).contains("tui"));
        let action = modal.actions()[0];
        assert_eq!(action, action);
        assert!(format!("{action:?}").contains("agent"));
    }

    #[test]
    fn selection_wraps_both_ways() {
        let mut modal = CloseupModal::new("s");
        modal.select_prev(); // wrap to last (terminal)
        assert_eq!(modal.selected(), 4);
        assert_eq!(modal.selected_action().name, "terminal");
        modal.select_next(); // wrap to 0
        assert_eq!(modal.selected(), 0);
        modal.select_next();
        assert_eq!(modal.selected_action().name, "close");
    }

    #[test]
    fn with_session_retitles_without_touching_input_state() {
        let mut modal = CloseupModal::new("old");
        modal.select_next(); // move off the default action
        let selected = modal.selected();
        let modal = modal.with_session("renamed");
        assert_eq!(modal.session(), "renamed");
        assert_eq!(modal.selected(), selected);
        // Exercise the derived structural equality used by the render projection.
        assert_eq!(modal.clone(), modal);
        assert_ne!(modal, CloseupModal::new("renamed"));
    }

    #[test]
    fn selected_action_submission_comes_from_the_registry() {
        let mut modal = CloseupModal::new("s");
        assert_eq!(modal.submission(), "agent");
        modal.select_next();
        assert_eq!(modal.submission(), "close");
    }

    #[test]
    fn an_out_of_range_selection_has_no_subcommands() {
        let mut modal = CloseupModal::new("s");
        modal.selected = usize::MAX;
        assert!(modal.subcommands().is_empty());
    }

    #[test]
    fn expanded_action_cycles_subcommands_and_renders_them() {
        let mut modal = CloseupModal::new("s");
        modal.select_next(); // close
        modal.expand_selected();
        assert_eq!(modal.submission(), "close --force");
        assert!(joined(&modal).contains("--force"));
        modal.select_next();
        modal.select_prev();
        assert!(modal.collapse());
        assert!(!modal.collapse());
        while modal.selected_action().name != "terminal" {
            modal.select_next();
        }
        modal.expand_selected();
        modal.select_next(); // second terminal subcommand
        assert!(joined(&modal).contains("      open"));
        assert!(joined(&modal).contains("› new"));
    }

    #[test]
    fn prompt_caret_can_move_in_both_directions() {
        let mut modal = CloseupModal::with_selection_mode("s", ModalSelectionMode::Prompt);
        modal.insert_char('a');
        modal.insert_char('b');
        modal.cursor_left();
        modal.cursor_right();
        assert_eq!(modal.submission(), "ab");
    }

    #[test]
    fn prompt_home_end_and_selection_edit_the_input() {
        let mut modal = CloseupModal::with_selection_mode("s", ModalSelectionMode::Prompt);
        for character in "close".chars() {
            modal.insert_char(character);
        }
        modal.cursor_home();
        modal.select_right();
        modal.select_right();
        assert_eq!(modal.selection(), Some((0, 2)));
        modal.select_end();
        assert_eq!(modal.selection(), Some((0, 5)));
        modal.delete_forward(); // drops the whole selection
        assert_eq!(modal.submission(), "");
        assert_eq!(modal.selection(), None);

        for character in "abc".chars() {
            modal.insert_char(character);
        }
        modal.cursor_end();
        modal.select_home(); // anchor 3, caret 0
        assert_eq!(modal.selection(), Some((0, 3)));
        modal.select_right(); // caret 1, shrinking the range from the left edge
        assert_eq!(modal.selection(), Some((1, 3)));
        modal.cursor_home(); // a non-selecting move clears the selection
        assert_eq!(modal.selection(), None);
    }

    #[test]
    fn prompt_mode_accepts_a_typed_command_instead_of_an_action_choice() {
        let mut modal = CloseupModal::with_selection_mode("s", ModalSelectionMode::Prompt);
        modal.insert_char('c');
        modal.insert_char('l');
        modal.insert_char('o');
        modal.backspace();
        assert_eq!(modal.selection_mode(), ModalSelectionMode::Prompt);
        assert_eq!(modal.submission(), "cl");
        assert!(joined(&modal).contains("Type a command:"));
        // Closeup prompt uses the same block-caret renderer as other TextInput views.
        assert!(
            render(24, 80, &modal)
                .join("\n")
                .contains("\u{1b}[7;36m \u{1b}[0m")
        );
    }

    #[test]
    fn tab_completes_closeup_commands_and_unambiguous_subcommands() {
        let mut modal = CloseupModal::with_selection_mode("s", ModalSelectionMode::Prompt);
        for character in "ter".chars() {
            modal.insert_char(character);
        }
        modal.complete_selected();
        assert_eq!(modal.input.value(), "terminal");

        modal = CloseupModal::with_selection_mode("s", ModalSelectionMode::Prompt);
        for character in "terminal n".chars() {
            modal.insert_char(character);
        }
        modal.complete_selected();
        assert_eq!(modal.input.value(), "terminal new");
    }

    #[test]
    fn tab_without_a_closeup_candidate_preserves_the_entire_input_state() {
        for input in ["terminal ", "agent x", "terminal new extra"] {
            let mut modal = CloseupModal::with_selection_mode("s", ModalSelectionMode::Prompt);
            for character in input.chars() {
                modal.insert_char(character);
            }
            modal.selected = 3;
            modal.cursor_left();
            let input = modal.input.value().to_owned();
            let cursor = modal.input.cursor();
            let selected = modal.selected;

            modal.complete_selected();

            assert_eq!(modal.input.value(), input);
            assert_eq!(modal.input.cursor(), cursor);
            assert_eq!(modal.selected, selected);
        }
    }

    #[test]
    fn render_shows_the_session_actions_and_footer() {
        let text = joined(&CloseupModal::new("daemon"));
        assert!(text.contains("Closeup: daemon")); // タイトル
        assert!(text.contains("Run a command:"));
        assert!(text.contains("terminal"));
        assert!(text.contains("Launch or attach"));
        assert!(text.contains("close"));
        assert!(text.contains("Enter: run"));
        assert!(text.contains("Esc: back"));
        // 選択マーカーは 1 つ。
        assert!(text.contains('›'));
    }

    #[test]
    fn render_marks_the_selected_action() {
        let mut modal = CloseupModal::new("s");
        modal.select_next(); // Focus agent
        let frame = render(24, 80, &modal);
        let selection_rows = frame
            .iter()
            .filter(|line| strip_ansi(line).contains('›'))
            .count();
        assert_eq!(selection_rows, 1);
        assert!(!frame.iter().any(|line| strip_ansi(line).contains('❯')));
    }

    #[test]
    fn render_fills_the_terminal() {
        let frame = render(24, 80, &CloseupModal::new("s"));
        assert_eq!(frame.len(), 24);
        assert!(frame.iter().all(|l| display_width(l) <= 80));
        // サイズ 0 は 80×24 にフォールバック。
        assert_eq!(render(0, 0, &CloseupModal::new("s")).len(), 24);
    }

    #[test]
    fn render_over_keeps_the_workspace_background_visible() {
        let base: Vec<String> = (0..24)
            .map(|row| format!("workspace-row-{row}-{}", ".".repeat(80)))
            .collect();
        let frame = render_over(24, 80, &base, &CloseupModal::new("daemon"));
        let text = frame.join("\n");

        assert_eq!(frame.len(), 24);
        assert!(frame.iter().all(|line| display_width(line) == 80));
        assert!(frame[0].starts_with("workspace-row-0-"));
        assert!(text.contains("Closeup: daemon"));
        let modal_row = frame.iter().find(|line| line.contains('┌')).unwrap();
        assert!(modal_row.starts_with("workspace"));
        assert!(modal_row.trim_end().ends_with('.'));
    }

    #[test]
    fn render_over_fits_ansi_cjk_background_on_a_narrow_terminal() {
        let base = vec![format!("\u{1b}[35m{}\u{1b}[0m", "背景".repeat(8)); 14];
        let frame = render_over(14, 9, &base, &CloseupModal::new("会話"));

        assert_eq!(frame.len(), 14);
        assert!(frame.iter().all(|line| display_width(line) == 9));
        assert!(frame.iter().any(|line| line.contains('┌')));
        assert!(frame.iter().any(|line| line.contains("\u{1b}[35m")));
    }
}

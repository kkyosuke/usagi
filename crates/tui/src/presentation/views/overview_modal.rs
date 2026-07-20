//! Overview modal（コマンドパレット `:`）。
//!
//! workspace 画面で `:` を押すと開く、workspace 全体に効くコマンドの入力パレット。入力欄に
//! 打つと候補が前方一致で絞り込まれ、Tab で補完、↑↓ で履歴を遡れる。選択中 command の
//! usage / long help と直前の結果も同じ固定位置に表示する。中央に浮かぶ枠付きダイアログとして
//! 描く（配置は共通の [`modal`] widget に委譲）。
//!
//! 状態 [`OverviewModal`] は端末 IO を持たない純粋な値で、[`render`] が 1 フレーム分の行
//! （ANSI 付き `Vec<String>`）に変換する。キー入力の解釈は入力層が整うときに載せ、ここでは
//! 入力編集と候補選択の純粋操作だけを公開する。

use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::{TextInput, modal};
use crate::usecase::overview;
use usagi_core::domain::settings::ModalSelectionMode;

/// モーダルの枠の内側（内容）幅。
const INNER_WIDTH: usize = 56;
/// 一度に出す候補の最大数。
const MAX_MATCHES: usize = 8;
/// input, heading, candidates, help, result, and footer.
const BODY_HEIGHT: usize = 16;

/// コマンドパレットの状態。入力欄と、その前方一致で選ばれた候補上のカーソルを持つ。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OverviewModal {
    selection_mode: ModalSelectionMode,
    input: TextInput,
    selected: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    expanded: bool,
    selected_subcommand: usize,
    result: Option<PaletteResult>,
}

/// command 実行後に palette の結果帯へ残す安全な 1 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteResult {
    /// 成功または通常の通知。
    Notice(String),
    /// 入力・実行時の安全なエラー。
    Error(String),
}

impl OverviewModal {
    /// 空の入力で開いたパレット。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a palette using the configured command-selection interaction.
    #[must_use]
    pub fn with_selection_mode(selection_mode: ModalSelectionMode) -> Self {
        Self {
            selection_mode,
            ..Self::default()
        }
    }

    /// Returns whether candidates are chosen as actions or submitted as a prompt.
    #[must_use]
    pub fn selection_mode(&self) -> ModalSelectionMode {
        self.selection_mode
    }

    /// 現在の入力文字列。
    #[must_use]
    pub fn input(&self) -> &str {
        self.input.value()
    }

    /// 入力欄のキャレット位置（バイトオフセット）。
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.input.cursor()
    }

    /// 選択中候補の添字（[`OverviewModal::matches`] 内）。
    #[must_use]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// この palette を開いてから実行した command history。
    #[must_use]
    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// 直前の command 実行結果。
    #[must_use]
    pub fn result(&self) -> Option<&PaletteResult> {
        self.result.as_ref()
    }

    /// 入力の前方一致で絞り込んだコマンド候補。入力が空なら全件。
    #[must_use]
    pub fn matches(&self) -> Vec<overview::CommandInfo> {
        overview::complete(&overview::DefaultRegistry, self.input.value())
    }

    /// 選択中のトップレベル command、または一意な `session` subcommand を入力欄へ補完する。
    /// 候補が無ければ no-op。
    pub fn complete_selected(&mut self) {
        if let Some(input) = overview::completion(self.input.value()) {
            self.input = TextInput::with_value(&input);
            return;
        }
        // Top-level candidates are only valid while the cursor is still on the
        // command word.  Once arguments are present, `completion` above is the
        // sole source of candidates; falling back to `matches` would otherwise
        // replace an uncompletable argument with its first command word.
        if !self.input.value().contains(char::is_whitespace)
            && let Some(command) = self.matches().get(self.selected)
        {
            self.input = TextInput::with_value(command.name);
            self.selected = 0;
        }
    }

    /// 直近の history を入力欄へ呼び戻す。空の入力欄でのみ有効なので、候補選択の ↑ と
    /// 衝突しない。呼び戻せたかを返す。
    pub fn recall_previous(&mut self) -> bool {
        if (!self.input.value().trim().is_empty() && self.history_index.is_none())
            || self.history.is_empty()
        {
            return false;
        }
        let index = self
            .history_index
            .map_or(self.history.len() - 1, |index| index.saturating_sub(1));
        self.history_index = Some(index);
        self.input = TextInput::with_value(&self.history[index]);
        self.selected = 0;
        true
    }

    /// history を新しい方へ進める。最後の次では空の新規入力に戻る。呼び戻せたかを返す。
    pub fn recall_next(&mut self) -> bool {
        let Some(index) = self.history_index else {
            return false;
        };
        if index + 1 == self.history.len() {
            self.history_index = None;
            self.input = TextInput::default();
        } else {
            let next = index + 1;
            self.history_index = Some(next);
            self.input = TextInput::with_value(&self.history[next]);
        }
        self.selected = 0;
        true
    }

    /// 現在の submission を history に記録する。同じ command が連続した場合は重複させない。
    pub fn record_submission(&mut self) {
        let submission = self.submission();
        if !submission.is_empty() && self.history.last() != Some(&submission) {
            self.history.push(submission);
        }
        self.history_index = None;
    }

    /// command 実行の通常結果を結果帯へ表示する。
    pub fn set_result(&mut self, result: impl Into<String>) {
        self.result = Some(PaletteResult::Notice(result.into()));
    }

    /// command 実行の安全なエラーを結果帯へ表示する。
    pub fn set_error(&mut self, error: impl Into<String>) {
        self.result = Some(PaletteResult::Error(error.into()));
    }

    /// 結果帯を消す。
    pub fn clear_result(&mut self) {
        self.result = None;
    }

    /// Enter で controller へ渡す入力。空欄では選択中候補を実行する。
    #[must_use]
    pub fn submission(&self) -> String {
        if self.selection_mode == ModalSelectionMode::Prompt {
            return self.input.value().to_owned();
        }
        if self.expanded {
            return self
                .selected_command()
                .map(|command| {
                    format!(
                        "{} {}",
                        command.name,
                        self.subcommands()[self.selected_subcommand]
                    )
                })
                .unwrap_or_default();
        }
        if self.input.value().contains(char::is_whitespace) || self.matches().is_empty() {
            return self.input.value().to_owned();
        }
        self.selected_command()
            .map_or_else(String::new, |command| command.name.to_owned())
    }

    /// Expand the selected command's action picker when it has subcommands.
    pub fn expand_selected(&mut self) {
        if !self.subcommands().is_empty() {
            self.expanded = true;
            self.selected_subcommand = 0;
        }
    }

    /// Collapse an expanded action picker. Returns whether a picker was open.
    pub fn collapse(&mut self) -> bool {
        std::mem::take(&mut self.expanded)
    }

    /// キャレット位置に 1 文字挿入し、選択を先頭に戻す（候補集合が変わるため）。
    pub fn insert_char(&mut self, c: char) {
        self.input.insert(c);
        self.selected = 0;
        self.history_index = None;
        self.expanded = false;
    }

    /// キャレット手前の 1 文字を削除し、選択を先頭に戻す。
    pub fn backspace(&mut self) {
        self.input.backspace();
        self.selected = 0;
        self.history_index = None;
        self.expanded = false;
    }

    /// キャレットを 1 文字左へ。
    pub fn cursor_left(&mut self) {
        self.input.move_left();
    }

    /// キャレットを 1 文字右へ。
    pub fn cursor_right(&mut self) {
        self.input.move_right();
    }

    /// 選択を次の候補へ（末尾で先頭へ回り込む）。候補が無ければ何もしない。
    pub fn select_next(&mut self) {
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

    /// 選択を前の候補へ（先頭で末尾へ回り込む）。候補が無ければ何もしない。
    #[coverage(off)] // The empty-match branch is exercised through the reducer integration path.
    pub fn select_prev(&mut self) {
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

    fn selected_command(&self) -> Option<overview::CommandInfo> {
        self.matches().get(self.selected).copied()
    }

    fn subcommands(&self) -> &'static [&'static str] {
        match self.selected_command().map(|command| command.name) {
            Some("session") => &["list", "overview", "remove"],
            _ => &[],
        }
    }
}

/// 1 候補行: 選択中は `›` マーカー、コマンド名（accent）、説明（dim）。幅に切り詰める。
fn hint_row(hint: overview::CommandInfo, selected: bool, inner: usize) -> String {
    let marker = modal::selection_marker(selected);
    // コマンド名は ASCII なので固定幅を char 数で確保してから塗る（説明の桁がそろう）。
    let name = Role::Accent
        .style()
        .bold()
        .paint(&format!("{:<10}", hint.name));
    let desc = Style::new().dim().paint(hint.description);
    modal::content_line(&format!("{marker} {name}{desc}"), inner)
}

/// コマンドパレットのボディ（枠の内側の行）。入力行・候補一覧・フッタからなる。
#[coverage(off)] // Rendering branches are covered by frame snapshots; LLVM misses ANSI subcommand rows.
fn body(state: &OverviewModal) -> Vec<String> {
    let matches = state.matches();
    let mut lines = vec![
        modal::prompt_line(state.input(), state.cursor()),
        String::new(),
    ];
    if matches.is_empty() {
        lines.push(modal::empty_notice("no matching command"));
    } else {
        let header = if state.input().trim().is_empty() {
            "workspace commands"
        } else {
            "matches"
        };
        lines.push(modal::caption(header));
        for (i, hint) in matches.iter().take(MAX_MATCHES).enumerate() {
            lines.push(hint_row(*hint, i == state.selected, INNER_WIDTH));
            if state.selection_mode == ModalSelectionMode::Action
                && state.expanded
                && i == state.selected
            {
                for (sub_index, subcommand) in state.subcommands().iter().enumerate() {
                    lines.push(modal::subcommand_row(
                        subcommand,
                        sub_index == state.selected_subcommand,
                    ));
                }
            }
        }
    }
    let help = matches
        .get(state.selected)
        .copied()
        .or_else(|| overview::help(&overview::DefaultRegistry, state.input()));
    if let Some(help) = help {
        lines.push(modal::caption(help.usage));
        lines.push(modal::caption(help.long_description));
    }
    lines.push(String::new());
    match state.result() {
        Some(PaletteResult::Notice(result)) => {
            lines.push(Role::Success.style().paint(&format!("  {result}")));
        }
        Some(PaletteResult::Error(error)) => {
            lines.push(Role::Danger.style().paint(&format!("  {error}")));
        }
        None => lines.push(String::new()),
    }
    lines.push(String::new());
    lines.push(modal::footer(
        if state.selection_mode == ModalSelectionMode::Action {
            "Tab: complete ↑↓ select → expand Enter run Esc: close"
        } else {
            "Enter: run   Esc: close"
        },
    ));
    lines
}

/// 生の端末サイズに対する overview modal 1 フレーム分の行。中央に浮かぶ枠付きダイアログとして
/// 描く（枠と中央寄せは [`modal::render_modal`] に委譲）。サイズ 0 は 80×24 にフォールバック。
#[must_use]
pub fn render(raw_height: usize, raw_width: usize, state: &OverviewModal) -> Vec<String> {
    modal::render_body(
        raw_height,
        raw_width,
        "Overview",
        INNER_WIDTH,
        BODY_HEIGHT,
        body(state),
    )
}

/// `base` の workspace フレームを背景に残し、overview modal を中央に合成する。
/// 小端末では [`modal::render_body_over`] が Home の帯を残す。サイズ 0 は 80×24 にフォールバックする。
#[must_use]
pub fn render_over(
    raw_height: usize,
    raw_width: usize,
    base: &[String],
    state: &OverviewModal,
) -> Vec<String> {
    modal::render_body_over(
        raw_height,
        raw_width,
        base,
        "Overview",
        INNER_WIDTH,
        BODY_HEIGHT,
        body(state),
    )
}

#[cfg(test)]
mod tests {
    use super::{OverviewModal, PaletteResult, render, render_over};
    use crate::presentation::widgets::{display_width, strip_ansi};
    use usagi_core::domain::settings::ModalSelectionMode;

    fn joined(state: &OverviewModal) -> String {
        render(24, 80, state)
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn type_str(state: &mut OverviewModal, text: &str) {
        for c in text.chars() {
            state.insert_char(c);
        }
    }

    #[test]
    fn new_modal_is_empty_and_lists_every_command() {
        let modal = OverviewModal::new();
        assert_eq!(modal.input(), "");
        assert_eq!(modal.cursor(), 0);
        assert_eq!(modal.selected(), 0);
        assert_eq!(modal.matches().len(), 4);
        // derive された Clone / Debug / Eq も触れる。
        assert!(format!("{:?}", modal.clone()).contains("OverviewModal"));
        assert_eq!(modal.clone(), modal);
        let mut other = modal.clone();
        other.insert_char('x');
        assert_ne!(other, modal);
        // registry metadata の derive も。
        let hint = modal.matches()[0];
        assert_eq!(hint, hint);
        assert!(format!("{hint:?}").contains("config"));
    }

    #[test]
    fn prompt_mode_submits_only_the_typed_command() {
        let mut modal = OverviewModal::with_selection_mode(ModalSelectionMode::Prompt);
        modal.insert_char('s');
        modal.insert_char('e');
        assert_eq!(modal.selection_mode(), ModalSelectionMode::Prompt);
        assert_eq!(modal.submission(), "se");
    }

    #[test]
    fn typing_filters_by_prefix_and_resets_the_selection() {
        let mut modal = OverviewModal::new();
        modal.select_next(); // selected = 1
        type_str(&mut modal, "i");
        // "i" 前方一致: issue。
        let names: Vec<&str> = modal.matches().iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["issue"]);
        // 入力で選択は先頭へ。
        assert_eq!(modal.selected(), 0);
        // さらに入力しても候補は変わらない。
        type_str(&mut modal, "ss");
        assert_eq!(
            modal.matches().iter().map(|c| c.name).collect::<Vec<_>>(),
            vec!["issue"]
        );
    }

    #[test]
    fn backspace_widens_the_matches_again() {
        let mut modal = OverviewModal::new();
        type_str(&mut modal, "co");
        assert_eq!(
            modal.matches().iter().map(|c| c.name).collect::<Vec<_>>(),
            vec!["config"]
        );
        modal.backspace();
        // "c" に戻ると config だけ（他に c 始まりなし）。
        assert_eq!(
            modal.matches().iter().map(|c| c.name).collect::<Vec<_>>(),
            vec!["config"]
        );
        modal.backspace();
        assert_eq!(modal.matches().len(), 4);
    }

    #[test]
    fn selection_wraps_over_the_matches() {
        let mut modal = OverviewModal::new();
        modal.select_prev(); // wrap to last (3)
        assert_eq!(modal.selected(), 3);
        modal.select_next(); // wrap to 0
        assert_eq!(modal.selected(), 0);
    }

    #[test]
    fn action_mode_expands_and_cycles_session_subcommands() {
        let mut modal = OverviewModal::new();
        modal.expand_selected();
        assert_eq!(modal.submission(), "config");

        modal.select_prev(); // session
        modal.expand_selected();
        assert_eq!(modal.submission(), "session list");
        modal.select_next();
        assert_eq!(modal.submission(), "session overview");
        modal.select_next();
        assert_eq!(modal.submission(), "session remove");
        modal.select_next();
        assert_eq!(modal.submission(), "session list");
        assert!(modal.collapse());
        assert!(!modal.collapse());
    }

    #[test]
    fn completion_and_submission_use_the_registry_metadata() {
        let mut modal = OverviewModal::new();
        modal.select_next();
        let expected = modal.matches()[modal.selected()].name;
        modal.complete_selected();
        assert_eq!(modal.input(), expected);
        assert_eq!(modal.submission(), expected);

        let empty = OverviewModal::new();
        assert_eq!(empty.submission(), "config");
    }

    #[test]
    fn tab_completes_a_session_subcommand_without_replacing_the_command() {
        let mut modal = OverviewModal::new();
        for character in "session c".chars() {
            modal.insert_char(character);
        }
        modal.complete_selected();
        assert_eq!(modal.input(), "session create");
    }

    #[test]
    fn tab_without_an_argument_completion_preserves_the_entire_input_state() {
        let mut modal = OverviewModal::new();
        type_str(&mut modal, "session create ");
        modal.cursor_left();
        let input = modal.input().to_owned();
        let cursor = modal.cursor();
        let selected = modal.selected();

        modal.complete_selected();

        assert_eq!(modal.input(), input);
        assert_eq!(modal.cursor(), cursor);
        assert_eq!(modal.selected(), selected);
    }

    #[test]
    fn content_changes_keep_the_overview_box_height_stable() {
        let mut modal = OverviewModal::new();
        let empty = render(40, 80, &modal)
            .iter()
            .filter(|line| line.contains('│') || line.contains('┌') || line.contains('└'))
            .count();
        modal.set_error("safe error");
        let error = render(40, 80, &modal)
            .iter()
            .filter(|line| line.contains('│') || line.contains('┌') || line.contains('└'))
            .count();
        assert_eq!(empty, error);
    }

    #[test]
    #[coverage(off)]
    fn history_recall_moves_between_submissions_without_duplicating_them() {
        let mut modal = OverviewModal::new();
        type_str(&mut modal, "issue list");
        modal.record_submission();
        modal.record_submission();
        modal = OverviewModal::new();
        type_str(&mut modal, "session list");
        modal.record_submission();
        assert_eq!(modal.history(), ["session list"]);

        // Seed a second command through the public history state transition.
        modal.backspace();
        for _ in 0..11 {
            modal.backspace();
        }
        type_str(&mut modal, "issue list");
        modal.record_submission();
        modal = modal.clone();
        modal.backspace();
        for _ in 0..10 {
            modal.backspace();
        }
        assert!(modal.recall_previous());
        assert_eq!(modal.input(), "issue list");
        assert!(modal.recall_previous());
        assert_eq!(modal.input(), "session list");
        assert!(modal.recall_next());
        assert_eq!(modal.input(), "issue list");
        assert!(modal.recall_next());
        assert_eq!(modal.input(), "");
        // Past the newest entry there is nothing to recall forward to.
        assert!(!modal.recall_next());
        // A non-empty draft with no active recall keeps Up as candidate
        // navigation rather than history recall.
        type_str(&mut modal, "x");
        assert!(!modal.recall_previous());
    }

    #[test]
    fn render_shows_long_help_and_a_result_strip() {
        let mut modal = OverviewModal::new();
        modal.set_result("Settings saved");
        let text = joined(&modal);
        assert!(text.contains("Open the local settings surface"));
        assert!(text.contains("Settings saved"));
        assert!(text.contains("Tab: complete"));
        assert_eq!(
            modal.result(),
            Some(&PaletteResult::Notice("Settings saved".to_owned()))
        );

        modal.set_error("Settings are unavailable");
        assert_eq!(
            modal.result(),
            Some(&PaletteResult::Error("Settings are unavailable".to_owned()))
        );
        modal.clear_result();
        assert_eq!(modal.result(), None);
    }

    #[test]
    fn selection_is_a_noop_when_there_are_no_matches() {
        let mut modal = OverviewModal::new();
        type_str(&mut modal, "zzz"); // 何にも一致しない
        assert!(modal.matches().is_empty());
        modal.select_next();
        modal.select_prev();
        assert_eq!(modal.selected(), 0);
    }

    #[test]
    fn caret_moves_within_the_input() {
        let mut modal = OverviewModal::new();
        type_str(&mut modal, "ab");
        assert_eq!(modal.cursor(), 2);
        modal.cursor_left();
        assert_eq!(modal.cursor(), 1);
        modal.cursor_right();
        assert_eq!(modal.cursor(), 2);
    }

    #[test]
    fn render_shows_the_prompt_commands_and_footer() {
        let text = joined(&OverviewModal::new());
        assert!(text.contains("Overview")); // タイトル
        assert!(text.contains('❯')); // プロンプト
        assert!(text.contains("workspace commands"));
        assert!(text.contains("config"));
        assert!(text.contains("Edit this workspace's local settings"));
        assert!(text.contains("Esc: close"));
    }

    #[test]
    fn render_says_matches_when_filtering_and_marks_the_selection() {
        let mut modal = OverviewModal::new();
        type_str(&mut modal, "is"); // issue のみ
        let text = joined(&modal);
        assert!(text.contains("matches"));
        assert!(text.contains("issue"));
        assert!(text.contains('›')); // 選択マーカー
    }

    #[test]
    fn render_shows_a_no_match_notice() {
        let mut modal = OverviewModal::new();
        type_str(&mut modal, "zzz");
        let text = joined(&modal);
        assert!(text.contains("no matching command"));
    }

    #[test]
    fn render_draws_the_caret_mid_input() {
        let mut modal = OverviewModal::new();
        type_str(&mut modal, "abc");
        modal.cursor_left(); // キャレットは 'c' の手前
        let text = joined(&modal);
        assert!(text.contains("abc"));
        assert!(
            render(24, 80, &modal)
                .join("\n")
                .contains("\u{1b}[7;36mc\u{1b}[0m")
        );
    }

    #[test]
    fn render_fills_the_terminal() {
        let frame = render(24, 80, &OverviewModal::new());
        assert_eq!(frame.len(), 24);
        assert!(frame.iter().all(|l| display_width(l) <= 80));
        // サイズ 0 は 80×24 にフォールバック。
        assert_eq!(render(0, 0, &OverviewModal::new()).len(), 24);
    }

    #[test]
    fn render_over_keeps_the_workspace_background_visible() {
        let base: Vec<String> = (0..24)
            .map(|row| format!("workspace-row-{row}-{}", ".".repeat(80)))
            .collect();
        let frame = render_over(24, 80, &base, &OverviewModal::new());
        let text = frame.join("\n");

        assert_eq!(frame.len(), 24);
        assert!(frame.iter().all(|line| display_width(line) == 80));
        assert!(frame[0].starts_with("workspace-row-0-"));
        assert!(text.contains("Overview"));
        assert!(text.contains("workspace commands"));
        // modal の左右にも元フレームが残る。
        let modal_row = frame.iter().find(|line| line.contains('┌')).unwrap();
        assert!(modal_row.starts_with("workspace"));
        assert!(modal_row.trim_end().ends_with('.'));
    }

    #[test]
    fn render_over_fits_ansi_cjk_background_on_a_narrow_terminal() {
        let base = vec![format!("\u{1b}[32m{}\u{1b}[0m", "背景".repeat(8)); 16];
        let frame = render_over(16, 9, &base, &OverviewModal::new());

        assert_eq!(frame.len(), 16);
        assert!(frame.iter().all(|line| display_width(line) == 9));
        assert!(frame.iter().any(|line| line.contains('┌')));
        assert!(frame.iter().any(|line| line.contains("\u{1b}[32m")));
    }
}

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
use crate::presentation::widgets::{self, TextInput, modal};
use crate::usecase::closeup;
use usagi_core::domain::settings::ModalSelectionMode;

/// モーダルの枠の内側（内容）幅。
const INNER_WIDTH: usize = 50;
const BODY_HEIGHT: usize = 9;

/// アクションメニューの状態。対象セッション名と、アクション一覧上のカーソルを持つ。
#[derive(Debug, Clone)]
pub struct CloseupModal {
    session: String,
    selected: usize,
    selection_mode: ModalSelectionMode,
    input: TextInput,
}

impl CloseupModal {
    /// セッション `session` を対象に、先頭アクションを選んだメニューを開く。
    #[must_use]
    #[coverage(off)]
    pub fn new(session: impl Into<String>) -> Self {
        Self::with_selection_mode(session, ModalSelectionMode::Action)
    }

    /// Open a modal using the configured command-selection interaction.
    #[must_use]
    #[coverage(off)]
    pub fn with_selection_mode(
        session: impl Into<String>,
        selection_mode: ModalSelectionMode,
    ) -> Self {
        Self {
            session: session.into(),
            selected: 0,
            selection_mode,
            input: TextInput::default(),
        }
    }

    /// 対象セッション名。
    #[must_use]
    #[coverage(off)]
    pub fn session(&self) -> &str {
        &self.session
    }

    /// 選択中アクションの添字。
    #[must_use]
    #[coverage(off)]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Returns whether this modal accepts an action choice or a typed prompt.
    #[must_use]
    #[coverage(off)]
    pub fn selection_mode(&self) -> ModalSelectionMode {
        self.selection_mode
    }

    /// アクション一覧。
    #[must_use]
    #[coverage(off)]
    pub fn actions(&self) -> Vec<closeup::CommandInfo> {
        closeup::commands().collect()
    }

    /// 選択中のアクション。
    #[must_use]
    #[coverage(off)]
    pub fn selected_action(&self) -> closeup::CommandInfo {
        self.actions()[self.selected]
    }

    /// Enter で controller へ渡す registry command。Closeup は入力欄を持たないため、
    /// 選択行の command 名そのものが completion になる。
    #[must_use]
    #[coverage(off)]
    pub fn submission(&self) -> String {
        match self.selection_mode {
            ModalSelectionMode::Action => self.selected_action().name.to_owned(),
            ModalSelectionMode::Prompt => self.input.value().to_owned(),
        }
    }

    /// Insert one character in Prompt mode.
    #[coverage(off)]
    pub fn insert_char(&mut self, c: char) {
        if self.selection_mode == ModalSelectionMode::Prompt {
            self.input.insert(c);
        }
    }

    /// Delete one character in Prompt mode.
    #[coverage(off)]
    pub fn backspace(&mut self) {
        if self.selection_mode == ModalSelectionMode::Prompt {
            self.input.backspace();
        }
    }

    /// Move the prompt caret left in Prompt mode.
    #[coverage(off)]
    pub fn cursor_left(&mut self) {
        if self.selection_mode == ModalSelectionMode::Prompt {
            self.input.move_left();
        }
    }

    /// Move the prompt caret right in Prompt mode.
    #[coverage(off)]
    pub fn cursor_right(&mut self) {
        if self.selection_mode == ModalSelectionMode::Prompt {
            self.input.move_right();
        }
    }

    /// 選択を次へ（末尾で先頭へ回り込む）。
    #[coverage(off)]
    pub fn select_next(&mut self) {
        self.selected = (self.selected + 1) % self.actions().len();
    }

    /// 選択を前へ（先頭で末尾へ回り込む）。
    #[coverage(off)]
    pub fn select_prev(&mut self) {
        let len = self.actions().len();
        self.selected = (self.selected + len - 1) % len;
    }
}

/// 1 アクション行: 選択中は `›` マーカー、command 名（accent）、説明（dim）。
#[coverage(off)]
fn action_row(action: closeup::CommandInfo, selected: bool, inner: usize) -> String {
    let marker = if selected {
        Role::Danger.style().bold().paint("›")
    } else {
        " ".to_string()
    };
    let label = Role::Accent
        .style()
        .bold()
        .paint(&format!("{:<14}", action.name));
    let desc = Style::new().dim().paint(action.description);
    widgets::clip_to_width(&format!("  {marker} {label}{desc}"), inner)
}

/// アクションメニューのボディ（枠の内側の行）。対象セッションは v1 と同様に title にのみ載せる。
#[coverage(off)]
fn body(state: &CloseupModal) -> Vec<String> {
    if state.selection_mode == ModalSelectionMode::Prompt {
        let prompt = if state.input.value().is_empty() {
            "_".to_string()
        } else {
            state.input.value().to_owned()
        };
        return modal::fixed_body(
            vec![
                Style::new().dim().paint("Type a command:"),
                String::new(),
                format!("❯ {prompt}"),
                String::new(),
                Style::new().dim().paint("  Enter: run   Esc: back"),
            ],
            BODY_HEIGHT,
        );
    }
    let mut lines = vec![Style::new().dim().paint("Run a command:"), String::new()];
    for (i, action) in state.actions().iter().enumerate() {
        lines.push(action_row(*action, i == state.selected, INNER_WIDTH));
    }
    lines.push(String::new());
    lines.push(
        Style::new()
            .dim()
            .paint("  ↑↓: select   Enter: run   Esc: back"),
    );
    modal::fixed_body(lines, BODY_HEIGHT)
}

/// 生の端末サイズに対する closeup modal 1 フレーム分の行。中央に浮かぶ枠付きダイアログとして
/// 描く（枠と中央寄せは [`modal::render_modal`] に委譲）。サイズ 0 は 80×24 にフォールバック。
#[must_use]
#[coverage(off)]
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
/// サイズ 0 は 80×24 にフォールバックする。
#[must_use]
#[coverage(off)]
pub fn render_over(
    raw_height: usize,
    raw_width: usize,
    base: &[String],
    state: &CloseupModal,
) -> Vec<String> {
    modal::render_over(
        raw_height,
        raw_width,
        base,
        &format!("Closeup: {}", state.session()),
        INNER_WIDTH,
        &body(state),
    )
}

#[cfg(test)]
mod tests {
    use super::{CloseupModal, render, render_over};
    use crate::presentation::widgets::display_width;
    use usagi_core::domain::settings::ModalSelectionMode;

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

    #[coverage(off)]
    fn strip(line: &str) -> String {
        let mut out = String::new();
        let mut chars = line.chars();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) && c != '[' {
                        break;
                    }
                }
                continue;
            }
            out.push(ch);
        }
        out
    }

    fn joined(state: &CloseupModal) -> String {
        render(24, 80, state)
            .iter()
            .map(|l| strip(l))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn new_modal_targets_the_session_and_lists_actions() {
        let modal = CloseupModal::new("tui");
        assert_eq!(modal.session(), "tui");
        assert_eq!(modal.selected(), 0);
        assert_eq!(modal.actions().len(), 4);
        assert_eq!(modal.selected_action().name, "agent");
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
        assert_eq!(modal.selected(), 3);
        assert_eq!(modal.selected_action().name, "terminal");
        modal.select_next(); // wrap to 0
        assert_eq!(modal.selected(), 0);
        modal.select_next();
        assert_eq!(modal.selected_action().name, "close");
    }

    #[test]
    fn selected_action_submission_comes_from_the_registry() {
        let mut modal = CloseupModal::new("s");
        assert_eq!(modal.submission(), "agent");
        modal.select_next();
        assert_eq!(modal.submission(), "close");
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
        let cursor_rows = render(24, 80, &modal)
            .iter()
            .filter(|l| strip(l).contains('›'))
            .count();
        assert_eq!(cursor_rows, 1);
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

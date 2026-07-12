//! Closeup modal（セッションのアクションメニュー）。
//!
//! workspace 画面でフォーカス中のセッションに対する操作を選ぶ小さな中央メニュー。↑↓ で選び、
//! Enter で実行する。中央に浮かぶ枠付きダイアログとして描く（枠・配置は共通の [`modal`]
//! widget に委譲）。
//!
//! 状態 [`CloseupModal`] は端末 IO を持たない純粋な値で、[`render`] が 1 フレーム分の行
//! （ANSI 付き `Vec<String>`）に変換する。キー入力の解釈は入力層が整うときに載せ、ここでは
//! カーソル移動と選択の純粋操作だけを公開する。

use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::{self, modal};

/// モーダルの枠の内側（内容）幅。
const INNER_WIDTH: usize = 50;

/// メニューの 1 アクション（ダミー）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Action {
    /// 直接選ぶショートカット文字。
    pub key: char,
    /// 表示ラベル。
    pub label: &'static str,
    /// 1 行の説明。
    pub description: &'static str,
}

/// フォーカス中セッションに対して選べるアクション（ダミー）。
const ACTIONS: &[Action] = &[
    Action {
        key: 'o',
        label: "Open terminal",
        description: "attach the live shell",
    },
    Action {
        key: 'a',
        label: "Focus agent",
        description: "jump to the agent pane",
    },
    Action {
        key: 'n',
        label: "New tab",
        description: "open another pane",
    },
    Action {
        key: 'e',
        label: "Note",
        description: "edit the session note",
    },
    Action {
        key: 'x',
        label: "Close tab",
        description: "close the current pane",
    },
];

/// アクションメニューの状態。対象セッション名と、アクション一覧上のカーソルを持つ。
#[derive(Debug, Clone)]
pub struct CloseupModal {
    session: String,
    selected: usize,
}

impl CloseupModal {
    /// セッション `session` を対象に、先頭アクションを選んだメニューを開く。
    #[must_use]
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            selected: 0,
        }
    }

    /// 対象セッション名。
    #[must_use]
    pub fn session(&self) -> &str {
        &self.session
    }

    /// 選択中アクションの添字。
    #[must_use]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// アクション一覧。
    #[must_use]
    pub fn actions(&self) -> &'static [Action] {
        ACTIONS
    }

    /// 選択中のアクション。
    #[must_use]
    pub fn selected_action(&self) -> Action {
        ACTIONS[self.selected]
    }

    /// 選択を次へ（末尾で先頭へ回り込む）。
    pub fn select_next(&mut self) {
        self.selected = (self.selected + 1) % ACTIONS.len();
    }

    /// 選択を前へ（先頭で末尾へ回り込む）。
    pub fn select_prev(&mut self) {
        self.selected = (self.selected + ACTIONS.len() - 1) % ACTIONS.len();
    }

    /// ショートカット文字 `key` に対応するアクション。無ければ `None`。
    #[must_use]
    pub fn action_for(&self, key: char) -> Option<Action> {
        ACTIONS.iter().find(|a| a.key == key).copied()
    }
}

/// 1 アクション行: 選択中は `›` マーカー、`key` バッジ（warning）、ラベル（accent）、説明（dim）。
fn action_row(action: Action, selected: bool, inner: usize) -> String {
    let marker = if selected {
        Role::Danger.style().bold().paint("›")
    } else {
        " ".to_string()
    };
    let key = Role::Warning.style().bold().paint(&action.key.to_string());
    let label = Role::Accent
        .style()
        .bold()
        .paint(&format!("{:<14}", action.label));
    let desc = Style::new().dim().paint(action.description);
    widgets::clip_to_width(&format!("  {marker} {key}  {label}{desc}"), inner)
}

/// アクションメニューのボディ（枠の内側の行）: 対象セッションの見出し・アクション一覧・フッタ。
fn body(state: &CloseupModal) -> Vec<String> {
    let mut lines = vec![
        Style::new()
            .dim()
            .paint(&format!("session: {}", state.session())),
        String::new(),
    ];
    for (i, action) in ACTIONS.iter().enumerate() {
        lines.push(action_row(*action, i == state.selected, INNER_WIDTH));
    }
    lines.push(String::new());
    lines.push(Style::new().dim().paint("  Enter: run   Esc: close"));
    lines
}

/// 生の端末サイズに対する closeup modal 1 フレーム分の行。中央に浮かぶ枠付きダイアログとして
/// 描く（枠と中央寄せは [`modal::render_modal`] に委譲）。サイズ 0 は 80×24 にフォールバック。
#[must_use]
pub fn render(raw_height: usize, raw_width: usize, state: &CloseupModal) -> Vec<String> {
    modal::render_modal(raw_height, raw_width, "Session", INNER_WIDTH, &body(state))
}

#[cfg(test)]
mod tests {
    use super::{CloseupModal, render};
    use crate::presentation::widgets::display_width;

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
        assert_eq!(modal.actions().len(), 5);
        assert_eq!(modal.selected_action().key, 'o');
        // derive された Clone / Debug も触れる。
        assert!(format!("{:?}", modal.clone()).contains("tui"));
        let action = modal.actions()[0];
        assert_eq!(action, action);
        assert!(format!("{action:?}").contains("Open terminal"));
    }

    #[test]
    fn selection_wraps_both_ways() {
        let mut modal = CloseupModal::new("s");
        modal.select_prev(); // wrap to last (4)
        assert_eq!(modal.selected(), 4);
        assert_eq!(modal.selected_action().key, 'x');
        modal.select_next(); // wrap to 0
        assert_eq!(modal.selected(), 0);
        modal.select_next();
        assert_eq!(modal.selected_action().key, 'a');
    }

    #[test]
    fn action_for_maps_shortcut_keys() {
        let modal = CloseupModal::new("s");
        assert_eq!(
            modal.action_for('o').map(|a| a.label),
            Some("Open terminal")
        );
        assert_eq!(modal.action_for('x').map(|a| a.label), Some("Close tab"));
        assert_eq!(modal.action_for('z'), None);
    }

    #[test]
    fn render_shows_the_session_actions_and_footer() {
        let text = joined(&CloseupModal::new("daemon"));
        assert!(text.contains("Session")); // タイトル
        assert!(text.contains("session: daemon"));
        assert!(text.contains("Open terminal"));
        assert!(text.contains("attach the live shell"));
        assert!(text.contains("Close tab"));
        assert!(text.contains("Esc: close"));
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
}

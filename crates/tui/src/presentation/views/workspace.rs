//! Workspace 画面（ホーム）。
//!
//! workspace を開いている間の主画面。全幅の **header** の下を 2 ペインに割る:
//!
//! - 左ペイン **session menu** — セッション一覧（session）・root 行（root）・キー操作の footer。
//! - 右ペイン **closeup** — フォーカス中セッションの header・タブ切替の tabmenu・content・footer。
//!
//! 各パーツはそれぞれ独立した関数で組み、2 ペインの結合は共通の [`panes`] レイアウトに任せる。
//! 表示内容はダミー（[`Workspace::dummy`] が埋める）で、状態 [`Workspace`] は端末 IO を持たない
//! 純粋な値、[`render`] が 1 フレーム分の行（ANSI 付き `Vec<String>`）に変換する。
//!
//! キー入力の解釈は入力層が整うときに載せる。ここでは選択・タブ移動の純粋操作だけを公開する。

use crate::presentation::layouts::panes;
use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets;

/// 左ペイン（session menu）の希望表示幅。残りが右ペイン（closeup）になる。
const LEFT_WIDTH: usize = 28;
/// header・rule の 2 行を除いた本文（ペイン）領域の先頭からのオフセット。
const CHROME_ROWS: usize = 2;

/// 左ペインに並ぶ 1 セッション（ダミー）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// セッション名。
    pub name: String,
    /// 状態ラベル（ダミー: `● live` など）。
    pub status: &'static str,
}

/// 右ペインの 1 タブ（ダミー）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tab {
    /// タブのラベル。
    pub label: &'static str,
}

/// Workspace 画面の状態。表示はダミー。左ペインはセッション群＋末尾の root 行を選択でき、
/// 右ペインはタブを切り替えられる。
#[derive(Debug, Clone)]
pub struct Workspace {
    name: String,
    sessions: Vec<Session>,
    /// 選択行。`0..sessions.len()` はセッション、`sessions.len()` は root 行。
    selected: usize,
    tabs: Vec<Tab>,
    active_tab: usize,
}

impl Workspace {
    /// デモ用のダミー workspace（セッション 3 つ＋タブ 4 つ）。
    #[must_use]
    pub fn dummy() -> Self {
        Self {
            name: "usagi".to_string(),
            sessions: vec![
                Session {
                    name: "tui".to_string(),
                    status: "● live",
                },
                Session {
                    name: "daemon".to_string(),
                    status: "▶ running",
                },
                Session {
                    name: "docs".to_string(),
                    status: "◆ waiting",
                },
            ],
            selected: 0,
            tabs: vec![
                Tab { label: "Preview" },
                Tab { label: "Terminal" },
                Tab { label: "Diff" },
                Tab { label: "Notes" },
            ],
            active_tab: 0,
        }
    }

    /// workspace 名。
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// セッション一覧。
    #[must_use]
    pub fn sessions(&self) -> &[Session] {
        &self.sessions
    }

    /// タブ一覧。
    #[must_use]
    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    /// 選択行の添字（`sessions.len()` は root 行）。
    #[must_use]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// アクティブなタブの添字。
    #[must_use]
    pub fn active_tab(&self) -> usize {
        self.active_tab
    }

    /// root 行を選択しているか。
    #[must_use]
    pub fn root_selected(&self) -> bool {
        self.selected == self.sessions.len()
    }

    /// 左ペインの選択を 1 つ下へ（末尾の root の次は先頭へ回り込む）。
    pub fn select_next(&mut self) {
        self.selected = (self.selected + 1) % self.row_count();
    }

    /// 左ペインの選択を 1 つ上へ（先頭の次は末尾の root へ回り込む）。
    pub fn select_prev(&mut self) {
        let rows = self.row_count();
        self.selected = (self.selected + rows - 1) % rows;
    }

    /// 右ペインのタブを次へ（末尾で先頭へ回り込む）。
    pub fn tab_next(&mut self) {
        self.active_tab = (self.active_tab + 1) % self.tabs.len();
    }

    /// 右ペインのタブを前へ（先頭で末尾へ回り込む）。
    pub fn tab_prev(&mut self) {
        let len = self.tabs.len();
        self.active_tab = (self.active_tab + len - 1) % len;
    }

    /// 選択できる行数（セッション数＋root 行 1）。
    fn row_count(&self) -> usize {
        self.sessions.len() + 1
    }

    /// フォーカス中の行の表示名（root 選択なら "root"）。
    fn focused_name(&self) -> &str {
        self.sessions
            .get(self.selected)
            .map_or("root", |s| s.name.as_str())
    }
}

impl Default for Workspace {
    fn default() -> Self {
        Self::dummy()
    }
}

// ── header ──────────────────────────────────────────────────────────────────

/// 全幅の header: workspace 名のパンくずとセッション数（ダミー）。左寄せ・dim の区切り。
fn header_line(width: usize, ws: &Workspace) -> String {
    let count = ws.sessions.len();
    let sep = Style::new().dim().paint(" › ");
    let dot = Style::new().dim().paint(" · ");
    let line = format!(
        " {}{sep}{}{dot}{}",
        Role::Success.style().bold().paint("USAGI"),
        Role::Success.style().bold().paint(ws.name()),
        Style::new().dim().paint(&format!("{count} sessions")),
    );
    widgets::pad_to_width(&line, width)
}

/// header と本文を分ける全幅の水平罫線（dim）。
fn rule_line(width: usize) -> String {
    Style::new().dim().paint(&"─".repeat(width))
}

// ── left pane: session menu ───────────────────────────────────────────────────

/// セッション行（session）。選択中は `>` カーソル＋accent、状態ラベルは dim。
fn session_rows(width: usize, ws: &Workspace) -> Vec<String> {
    let mut rows = vec![Role::Success.style().bold().paint("Sessions")];
    for (i, session) in ws.sessions.iter().enumerate() {
        let selected = i == ws.selected;
        rows.push(menu_row(width, selected, &session.name, session.status));
    }
    rows
}

/// root 行（root）。フォーカス中は強調する。
fn root_row(width: usize, ws: &Workspace) -> String {
    menu_row(width, ws.root_selected(), "root", "workspace root")
}

/// 左ペインの 1 行: `>` カーソル＋名前（選択で accent 太字）＋dim の詳細。幅に詰める。
fn menu_row(width: usize, selected: bool, name: &str, detail: &str) -> String {
    let cursor = if selected {
        Role::Danger.style().bold().paint(">")
    } else {
        " ".to_string()
    };
    let name = if selected {
        Role::Accent.style().bold().paint(name)
    } else {
        name.to_string()
    };
    let detail = Style::new().dim().paint(detail);
    widgets::pad_to_width(&format!("{cursor} {name}  {detail}"), width)
}

/// 左ペインの footer（キー操作ヒント、dim）。
fn left_footer(width: usize) -> String {
    Style::new()
        .dim()
        .paint(&widgets::clip_to_width("↑↓ session  Enter  q quit", width))
}

/// 左ペイン（session menu）を `height` 行に組む: セッション群＋root 行、footer を最下行に固定。
fn left_pane(height: usize, width: usize, ws: &Workspace) -> Vec<String> {
    let mut rows = session_rows(width, ws);
    rows.push(String::new());
    rows.push(root_row(width, ws));
    with_footer(rows, height, left_footer(width))
}

// ── right pane: closeup ─────────────────────────────────────────────────────

/// closeup の header: フォーカス中セッションの名前と状態（ダミー）。
fn closeup_header(width: usize, ws: &Workspace) -> String {
    let name = Role::Accent.style().bold().paint(ws.focused_name());
    let detail = Style::new().dim().paint("branch: main · agent: idle");
    widgets::pad_to_width(&format!(" {name}  {detail}"), width)
}

/// tabmenu: タブ（tab）を並べ、アクティブを `[Label]` accent、他を dim で描く。
fn tab_menu(width: usize, ws: &Workspace) -> String {
    let tabs = ws
        .tabs
        .iter()
        .enumerate()
        .map(|(i, tab)| {
            if i == ws.active_tab {
                format!("[{}]", Role::Accent.style().bold().paint(tab.label))
            } else {
                format!(" {} ", Style::new().dim().paint(tab.label))
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    widgets::pad_to_width(&format!(" {tabs}"), width)
}

/// content: アクティブなタブに応じたダミー本文。
fn content_lines(ws: &Workspace) -> Vec<String> {
    let tab = ws.tabs.get(ws.active_tab).map_or("", |t| t.label);
    let session = ws.focused_name();
    vec![
        String::new(),
        Style::new()
            .dim()
            .paint(&format!("  {tab} — session '{session}' (dummy)")),
        String::new(),
        Style::new()
            .dim()
            .paint("  ここに選択中タブの内容が表示される。"),
    ]
}

/// 右ペインの footer（キー操作ヒント、dim）。
fn right_footer(width: usize) -> String {
    Style::new().dim().paint(&widgets::clip_to_width(
        "←→ tab / Enter open / Esc back",
        width,
    ))
}

/// 右ペイン（closeup）を `height` 行に組む: header・tabmenu・content、footer を最下行に固定。
fn right_pane(height: usize, width: usize, ws: &Workspace) -> Vec<String> {
    let mut rows = vec![
        closeup_header(width, ws),
        tab_menu(width, ws),
        String::new(),
    ];
    rows.extend(content_lines(ws));
    with_footer(rows, height, right_footer(width))
}

// ── composition ─────────────────────────────────────────────────────────────

/// `rows` を `height` 行に収め、`footer` を最下行に固定する（本文が溢れたら切り、足りなければ
/// 空行で詰める）。
fn with_footer(mut rows: Vec<String>, height: usize, footer: String) -> Vec<String> {
    let body_cap = height.saturating_sub(1);
    rows.truncate(body_cap);
    rows.resize(body_cap, String::new());
    rows.push(footer);
    rows.truncate(height);
    rows
}

/// 生の端末サイズに対する workspace 画面 1 フレーム分の行。全幅の header と罫線の下を、共通の
/// [`panes`] レイアウトで左（session menu）・右（closeup）の 2 ペインに割って組む。サイズ 0 は
/// 80×24 にフォールバックする。
#[must_use]
pub fn render(raw_height: usize, raw_width: usize, ws: &Workspace) -> Vec<String> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);

    let mut frame = Vec::with_capacity(height);
    frame.push(header_line(width, ws));
    frame.push(rule_line(width));

    let body_height = height.saturating_sub(CHROME_ROWS);
    let split = panes::split(width, LEFT_WIDTH);
    let left = left_pane(body_height, split.left, ws);
    let right = right_pane(body_height, split.right, ws);
    frame.extend(panes::join(body_height, &left, &right, split));

    frame.truncate(height);
    frame
}

#[cfg(test)]
mod tests {
    use super::{Workspace, render};
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

    fn joined(ws: &Workspace) -> String {
        render(30, 100, ws)
            .iter()
            .map(|l| strip(l))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn dummy_workspace_has_sessions_and_tabs() {
        let ws = Workspace::dummy();
        assert_eq!(ws.name(), "usagi");
        assert_eq!(ws.sessions().len(), 3);
        assert_eq!(ws.tabs().len(), 4);
        assert_eq!(ws.selected(), 0);
        assert_eq!(ws.active_tab(), 0);
        assert!(!ws.root_selected());
        // Default は dummy。derive された Clone / Debug も触れる。
        assert_eq!(Workspace::default().sessions().len(), 3);
        assert!(format!("{:?}", ws.clone()).contains("usagi"));
        // Session / Tab の derive も。
        assert_eq!(ws.sessions()[0], ws.sessions()[0]);
        assert_eq!(ws.tabs()[0], ws.tabs()[0]);
        assert!(format!("{:?}", ws.sessions()[0]).contains("tui"));
        assert!(format!("{:?}", ws.tabs()[0]).contains("Preview"));
    }

    #[test]
    fn select_cycles_through_sessions_then_the_root_row() {
        let mut ws = Workspace::dummy();
        ws.select_next(); // 1
        ws.select_next(); // 2
        assert_eq!(ws.selected(), 2);
        ws.select_next(); // 3 = root
        assert!(ws.root_selected());
        ws.select_next(); // wrap to 0
        assert_eq!(ws.selected(), 0);
        ws.select_prev(); // wrap to root
        assert!(ws.root_selected());
    }

    #[test]
    fn tab_navigation_wraps() {
        let mut ws = Workspace::dummy();
        ws.tab_prev(); // wrap to last (3)
        assert_eq!(ws.active_tab(), 3);
        ws.tab_next(); // wrap to 0
        assert_eq!(ws.active_tab(), 0);
        ws.tab_next();
        assert_eq!(ws.active_tab(), 1);
    }

    #[test]
    fn render_shows_the_header_and_both_panes() {
        let text = joined(&Workspace::dummy());
        // header: パンくずとセッション数。
        assert!(text.contains("USAGI"));
        assert!(text.contains("usagi"));
        assert!(text.contains("3 sessions"));
        // 左ペイン: セッション一覧・root。
        assert!(text.contains("Sessions"));
        assert!(text.contains("tui"));
        assert!(text.contains("daemon"));
        assert!(text.contains("root"));
        assert!(text.contains("q quit")); // 左 footer
        // 右ペイン: closeup header・tabmenu・content・footer。
        assert!(text.contains("Preview"));
        assert!(text.contains("Terminal"));
        assert!(text.contains("dummy")); // content
        assert!(text.contains("Esc back")); // 右 footer
        // 縦区切り。
        assert!(text.contains('│'));
    }

    #[test]
    fn render_reflects_the_selected_session_in_the_closeup_header() {
        let mut ws = Workspace::dummy();
        ws.select_next(); // daemon を選択
        let text = joined(&ws);
        // closeup header にフォーカス中セッション名。
        assert!(text.contains("daemon"));
    }

    #[test]
    fn render_marks_only_one_selected_row() {
        let text = joined(&Workspace::dummy());
        // 左ペインのカーソル `>` は 1 行だけ。
        let cursor_rows = render(30, 100, &Workspace::dummy())
            .iter()
            .filter(|l| strip(l).trim_start().starts_with('>'))
            .count();
        assert_eq!(cursor_rows, 1);
        assert!(text.contains('>'));
    }

    #[test]
    fn render_shows_root_focus_in_the_closeup_header() {
        let mut ws = Workspace::dummy();
        // root 行まで移動（セッション 3 つの次）。
        for _ in 0..3 {
            ws.select_next();
        }
        assert!(ws.root_selected());
        let text = joined(&ws);
        assert!(text.contains("root"));
    }

    #[test]
    fn render_fills_the_terminal_and_fits_its_width() {
        let frame = render(30, 100, &Workspace::dummy());
        assert_eq!(frame.len(), 30);
        assert!(frame.iter().all(|l| display_width(l) == 100));
    }

    #[test]
    fn render_falls_back_for_a_zero_size() {
        let frame = render(0, 0, &Workspace::dummy());
        assert_eq!(frame.len(), 24);
        assert!(frame.iter().all(|l| display_width(l) == 80));
    }

    #[test]
    fn render_does_not_overflow_a_short_terminal() {
        // header と罫線だけで埋まる高さでも溢れず、ちょうど height 行。
        let frame = render(2, 80, &Workspace::dummy());
        assert_eq!(frame.len(), 2);
        let tiny = render(1, 80, &Workspace::dummy());
        assert_eq!(tiny.len(), 1);
    }
}

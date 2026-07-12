//! Workspace 画面（ホーム）。
//!
//! workspace を開いている間の主画面。全幅の **header** の下を 2 ペインに割る:
//!
//! - 左ペイン **session menu** — セッション一覧（session）・root 行（root）・キー操作の footer。
//! - 右ペイン **closeup** — フォーカス中セッションの header・タブ切替の tabmenu・content・footer。
//!
//! 状態 [`Workspace`] は core の workspace と永続化済み [`WorkspaceState`] から構築する、端末 IO を
//! 持たない純粋な値である。[`render`] が 1 フレーム分の行（ANSI 付き `Vec<String>`）に変換する。

use std::path::Path;

use usagi_core::domain::pullrequest::PrLink;
use usagi_core::domain::session::SessionRecord;
use usagi_core::domain::workspace::Workspace as WorkspaceRecord;
use usagi_core::domain::workspace_state::WorkspaceState;

use crate::presentation::layouts::panes;
use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets;

/// 左ペイン（session menu）の希望表示幅。残りが右ペイン（closeup）になる。
const LEFT_WIDTH: usize = 28;
/// header・rule の 2 行を除いた本文（ペイン）領域の先頭からのオフセット。
const CHROME_ROWS: usize = 2;

/// Workspace 画面でキーボードが操作する対象。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// セッション一覧から操作対象を選ぶ。
    Switch,
    /// 選択中セッションのタブやアクションを操作する。
    Closeup,
}

impl Mode {
    const ALL: [Self; 2] = [Self::Switch, Self::Closeup];

    fn label(self) -> &'static str {
        match self {
            Self::Switch => "Switch",
            Self::Closeup => "Closeup",
        }
    }
}

/// 右ペインの 1 タブ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tab {
    /// タブのラベル。
    pub label: &'static str,
}

/// Workspace 画面の状態。左ペインは [`WorkspaceState`] のセッション群＋末尾の root 行を
/// 選択でき、右ペインのタブは Switch / Closeup のどちらでも切り替えられる。
#[derive(Debug, Clone)]
pub struct Workspace {
    record: WorkspaceRecord,
    state: WorkspaceState,
    mode: Mode,
    /// 選択行。`0..sessions.len()` はセッション、`sessions.len()` は root 行。
    selected: usize,
    tabs: Vec<Tab>,
    active_tab: usize,
}

impl Workspace {
    /// core の workspace とその永続化済み状態から画面状態を作る。
    #[must_use]
    pub fn new(workspace: WorkspaceRecord, state: WorkspaceState) -> Self {
        Self {
            record: workspace,
            state,
            mode: Mode::Switch,
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
        &self.record.name
    }

    /// workspace の絶対パス。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.record.path
    }

    /// セッション一覧。
    #[must_use]
    pub fn sessions(&self) -> &[SessionRecord] {
        &self.state.sessions
    }

    /// 現在の操作 mode。
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// 選択中の session を操作する Closeup へ移る。
    ///
    /// session と tab の選択位置はそのまま維持する。
    pub fn enter_closeup(&mut self) {
        self.mode = Mode::Closeup;
    }

    /// session 一覧を操作する Switch へ戻る。
    ///
    /// session と tab の選択位置はそのまま維持する。
    pub fn enter_switch(&mut self) {
        self.mode = Mode::Switch;
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
        self.selected == self.state.sessions.len()
    }

    /// フォーカス中 session の表示ラベル。root 行では `"root"`。
    #[must_use]
    pub fn focused_label(&self) -> &str {
        self.focused_session()
            .map_or("root", SessionRecord::display_label)
    }

    /// フォーカス中 session に記録された Pull Request。root 行では空。
    #[must_use]
    pub fn focused_prs(&self) -> &[PrLink] {
        self.focused_session()
            .map_or(&[], |session| session.prs.as_slice())
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
        self.state.sessions.len() + 1
    }

    /// フォーカス中のセッション（root 選択なら `None`）。
    fn focused_session(&self) -> Option<&SessionRecord> {
        self.state.sessions.get(self.selected)
    }
}

// ── header ──────────────────────────────────────────────────────────────────

/// 全幅の header: workspace 名のパンくずとセッション数。左寄せ・dim の区切り。
fn header_line(width: usize, ws: &Workspace) -> String {
    let count = ws.sessions().len();
    let sep = Style::new().dim().paint(" › ");
    let dot = Style::new().dim().paint(" · ");
    let modes = Mode::ALL
        .iter()
        .map(|mode| {
            if *mode == ws.mode() {
                Role::Accent.style().bold().paint(mode.label())
            } else {
                Style::new().dim().paint(mode.label())
            }
        })
        .collect::<Vec<_>>()
        .join("  ");
    let line = format!(
        " {}{sep}{}{dot}{}{dot}{modes}",
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

// ── left pane: session menu ─────────────────────────────────────────────────

/// root 行。フォーカス中は強調する。
fn root_row(width: usize, ws: &Workspace) -> String {
    menu_row(width, ws.root_selected(), "root", "workspace root")
}

/// 選択可能な 1 行。`0..sessions.len()` は session、末尾は root。
fn selectable_row(width: usize, ws: &Workspace, index: usize) -> String {
    ws.sessions().get(index).map_or_else(
        || root_row(width, ws),
        |session| {
            menu_row(
                width,
                index == ws.selected,
                session.display_label(),
                session.origin.as_str(),
            )
        },
    )
}

/// `capacity` 行の viewport に選択行が必ず入るよう、先頭 index を決める。
fn viewport_start(selected: usize, row_count: usize, capacity: usize) -> usize {
    let visible = capacity.min(row_count);
    let max_start = row_count.saturating_sub(visible);
    selected
        .saturating_sub(visible.saturating_sub(1))
        .min(max_start)
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
fn left_footer(width: usize, ws: &Workspace) -> String {
    let hint = match ws.mode() {
        Mode::Switch => "[switch] ↑↓ session",
        Mode::Closeup => "[closeup] session selected",
    };
    Style::new()
        .dim()
        .paint(&widgets::clip_to_width(hint, width))
}

/// 左ペイン（session menu）を `height` 行に組む。footer を最下行に
/// 固定し、残りを viewport として選択中の session / root 行を常に表示する。
fn left_pane(height: usize, width: usize, ws: &Workspace) -> Vec<String> {
    if height == 0 {
        return Vec::new();
    }
    if height == 1 {
        return vec![selectable_row(width, ws, ws.selected)];
    }

    let body_capacity = height - 1;
    let show_heading = body_capacity > 1;
    let viewport_capacity = body_capacity - usize::from(show_heading);
    let start = viewport_start(ws.selected, ws.row_count(), viewport_capacity);
    let end = (start + viewport_capacity).min(ws.row_count());

    let mut rows = Vec::with_capacity(height);
    if show_heading {
        rows.push(Role::Success.style().bold().paint("Sessions"));
    }
    for index in start..end {
        // 全行が収まる場合だけ、session と root の間に余白を残す。
        if index == ws.sessions().len()
            && start == 0
            && end == ws.row_count()
            && viewport_capacity > ws.row_count()
        {
            rows.push(String::new());
        }
        rows.push(selectable_row(width, ws, index));
    }
    rows.resize(body_capacity, String::new());
    rows.push(left_footer(width, ws));
    rows
}

// ── right pane: closeup ─────────────────────────────────────────────────────

/// closeup の header: フォーカス中セッションの identity と origin。root では workspace path。
fn closeup_header(width: usize, ws: &Workspace) -> String {
    let name = Role::Accent.style().bold().paint(ws.focused_label());
    let detail = ws.focused_session().map_or_else(
        || ws.path().display().to_string(),
        |session| format!("{} · {}", session.name, session.origin),
    );
    let detail = Style::new().dim().paint(&detail);
    widgets::pad_to_width(&format!(" {name}  {detail}"), width)
}

/// tabmenu: タブを並べ、アクティブを `[Label]` accent、他を dim で描く。
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

/// content: アクティブなタブと、フォーカス中の実 workspace / session path。
fn content_lines(ws: &Workspace) -> Vec<String> {
    let tab = ws.tabs[ws.active_tab].label;
    let (kind, path) = ws.focused_session().map_or_else(
        || ("workspace", ws.path()),
        |session| ("session", session.root.as_path()),
    );
    vec![
        String::new(),
        Style::new()
            .dim()
            .paint(&format!("  {tab} — {kind} '{}'", ws.focused_label())),
        String::new(),
        Style::new().dim().paint(&format!("  {}", path.display())),
    ]
}

/// 右ペインの footer（キー操作ヒント、dim）。
fn right_footer(width: usize, ws: &Workspace) -> String {
    let hint = match ws.mode() {
        Mode::Switch => "←→ tab / Enter closeup / : commands / p PR / Esc back / q quit",
        Mode::Closeup => "←→ tab / ↑↓ action / : commands / p PR / Esc switch / q quit",
    };
    Style::new()
        .dim()
        .paint(&widgets::clip_to_width(hint, width))
}

/// 右ペイン（closeup）を `height` 行に組む: header・tabmenu・content、footer を最下行に固定。
fn right_pane(height: usize, width: usize, ws: &Workspace) -> Vec<String> {
    let mut rows = vec![
        closeup_header(width, ws),
        tab_menu(width, ws),
        String::new(),
    ];
    rows.extend(content_lines(ws));
    with_footer(rows, height, right_footer(width, ws))
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
    use super::{Mode, Workspace, render};
    use crate::presentation::widgets::display_width;
    use chrono::{DateTime, Utc};
    use std::path::PathBuf;
    use usagi_core::domain::note::Scratchpad;
    use usagi_core::domain::pullrequest::PrLink;
    use usagi_core::domain::session::{SessionOrigin, SessionRecord};
    use usagi_core::domain::workspace::Workspace as WorkspaceRecord;
    use usagi_core::domain::workspace_state::WorkspaceState;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-06-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn session(name: &str, display_name: Option<&str>, origin: SessionOrigin) -> SessionRecord {
        SessionRecord {
            name: name.to_string(),
            display_name: display_name.map(str::to_string),
            origin,
            started_from: None,
            root: PathBuf::from(format!("/tmp/actual/.usagi/sessions/{name}")),
            created_at: now(),
            last_active: None,
            notes: Scratchpad::default(),
            prs: Vec::new(),
        }
    }

    fn workspace() -> Workspace {
        let record = WorkspaceRecord::new("actual", "/tmp/actual");
        let state = WorkspaceState {
            sessions: vec![
                session("tui", Some("UI work"), SessionOrigin::Human),
                session("daemon", None, SessionOrigin::Mcp),
            ],
            root_notes: Scratchpad::default(),
            updated_at: now(),
        };
        Workspace::new(record, state)
    }

    fn workspace_with_sessions(count: usize) -> Workspace {
        let record = WorkspaceRecord::new("actual", "/tmp/actual");
        let state = WorkspaceState {
            sessions: (0..count)
                .map(|index| session(&format!("session-{index:02}"), None, SessionOrigin::Human))
                .collect(),
            root_notes: Scratchpad::default(),
            updated_at: now(),
        };
        Workspace::new(record, state)
    }

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
            .map(|line| strip(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn workspace_is_built_from_domain_records() {
        let ws = workspace();
        assert_eq!(ws.name(), "actual");
        assert_eq!(ws.path(), PathBuf::from("/tmp/actual"));
        assert_eq!(ws.sessions().len(), 2);
        assert_eq!(ws.sessions()[0].display_label(), "UI work");
        assert_eq!(ws.tabs().len(), 4);
        assert_eq!(ws.mode(), Mode::Switch);
        assert_eq!(ws.selected(), 0);
        assert_eq!(ws.active_tab(), 0);
        assert!(!ws.root_selected());
        assert!(format!("{:?}", ws.clone()).contains("actual"));
        assert!(format!("{:?}", ws.tabs()[0]).contains("Preview"));
        assert_eq!(ws.tabs()[0], ws.tabs()[0]);
    }

    #[test]
    fn select_cycles_through_sessions_then_the_root_row() {
        let mut ws = workspace();
        ws.select_next();
        assert_eq!(ws.selected(), 1);
        ws.select_next();
        assert!(ws.root_selected());
        ws.select_next();
        assert_eq!(ws.selected(), 0);
        ws.select_prev();
        assert!(ws.root_selected());
    }

    #[test]
    fn an_empty_workspace_selects_and_cycles_the_root_row() {
        let mut ws = Workspace::new(
            WorkspaceRecord::new("empty", "/tmp/empty"),
            WorkspaceState::new(),
        );
        assert!(ws.root_selected());
        ws.select_next();
        ws.select_prev();
        assert_eq!(ws.selected(), 0);
        let text = joined(&ws);
        assert!(text.contains("0 sessions"));
        assert!(text.contains("/tmp/empty"));
    }

    #[test]
    fn tab_navigation_wraps() {
        let mut ws = workspace();
        ws.tab_prev();
        assert_eq!(ws.active_tab(), 3);
        ws.tab_next();
        assert_eq!(ws.active_tab(), 0);
        ws.tab_next();
        assert_eq!(ws.active_tab(), 1);
        assert!(joined(&ws).contains("Terminal — session 'UI work'"));
    }

    #[test]
    fn mode_transitions_preserve_the_session_and_tab_selection() {
        let mut ws = workspace();
        ws.select_next();
        ws.tab_next();
        let selected = ws.selected();
        let active_tab = ws.active_tab();

        ws.enter_closeup();
        assert_eq!(ws.mode(), Mode::Closeup);
        assert_eq!(ws.selected(), selected);
        assert_eq!(ws.active_tab(), active_tab);

        ws.enter_switch();
        assert_eq!(ws.mode(), Mode::Switch);
        assert_eq!(ws.selected(), selected);
        assert_eq!(ws.active_tab(), active_tab);
        assert!(format!("{:?}", ws.mode()).contains("Switch"));
    }

    #[test]
    fn focused_label_and_pull_requests_follow_the_selected_session() {
        let mut ws = workspace();
        ws.state.sessions[0]
            .prs
            .push(PrLink::new(42, "https://example.com/pull/42"));

        assert_eq!(ws.focused_label(), "UI work");
        assert_eq!(ws.focused_prs()[0].number, 42);

        ws.select_next();
        assert_eq!(ws.focused_label(), "daemon");
        assert!(ws.focused_prs().is_empty());

        ws.select_next();
        assert!(ws.root_selected());
        assert_eq!(ws.focused_label(), "root");
        assert!(ws.focused_prs().is_empty());
    }

    #[test]
    fn header_shows_both_modes_and_highlights_the_current_one() {
        let mut ws = workspace();
        let switch_header = &render(30, 100, &ws)[0];
        assert!(switch_header.contains("\u{1b}[1;36mSwitch\u{1b}[0m"));
        assert!(switch_header.contains("\u{1b}[2mCloseup\u{1b}[0m"));

        ws.enter_closeup();
        let closeup_header = &render(30, 100, &ws)[0];
        assert!(closeup_header.contains("\u{1b}[2mSwitch\u{1b}[0m"));
        assert!(closeup_header.contains("\u{1b}[1;36mCloseup\u{1b}[0m"));
    }

    #[test]
    fn render_uses_mode_specific_footers_and_keeps_tabs_visible() {
        let mut ws = workspace();
        let switch = joined(&ws);
        assert!(switch.contains("[switch] ↑↓ session"));
        assert!(switch.contains("←→ tab"));
        assert!(switch.contains("Enter closeup"));
        assert!(switch.contains("p PR"));
        for label in ["Preview", "Terminal", "Diff", "Notes"] {
            assert!(switch.contains(label));
        }

        ws.tab_next();
        ws.enter_closeup();
        let closeup_frame = render(30, 100, &ws);
        let closeup = closeup_frame
            .iter()
            .map(|line| strip(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(closeup.contains("[closeup] session selected"));
        assert!(closeup.contains("←→ tab"));
        assert!(closeup.contains("Esc switch"));
        assert!(closeup.contains("↑↓ action"));
        assert!(closeup.contains("Terminal — session 'UI work'"));
        assert!(
            closeup_frame
                .iter()
                .any(|line| line.contains("[\u{1b}[1;36mTerminal\u{1b}[0m]"))
        );
    }

    #[test]
    fn render_shows_real_workspace_and_session_records() {
        let text = joined(&workspace());
        assert!(text.contains("USAGI"));
        assert!(text.contains("actual"));
        assert!(text.contains("2 sessions"));
        assert!(text.contains("Sessions"));
        assert!(text.contains("UI work"));
        assert!(text.contains("human"));
        assert!(text.contains("daemon"));
        assert!(text.contains("mcp"));
        assert!(text.contains("tui · human"));
        assert!(text.contains("/tmp/actual/.usagi/sessions/tui"));
        assert!(text.contains("root"));
        assert!(text.contains("Preview"));
        assert!(text.contains("Terminal"));
        assert!(text.contains("Esc back"));
        assert!(text.contains('│'));
    }

    #[test]
    fn render_reflects_selected_session_and_root() {
        let mut ws = workspace();
        ws.select_next();
        let session_text = joined(&ws);
        assert!(session_text.contains("daemon · mcp"));
        assert!(session_text.contains("/tmp/actual/.usagi/sessions/daemon"));

        ws.select_next();
        assert!(ws.root_selected());
        let root_text = joined(&ws);
        assert!(root_text.contains("Preview — workspace 'root'"));
        assert!(root_text.contains("/tmp/actual"));
    }

    #[test]
    fn render_marks_only_one_selected_row() {
        let frame = render(30, 100, &workspace());
        let cursor_rows = frame
            .iter()
            .filter(|line| strip(line).trim_start().starts_with('>'))
            .count();
        assert_eq!(cursor_rows, 1);
    }

    #[test]
    fn session_viewport_keeps_every_selection_and_the_root_visible() {
        let mut ws = workspace_with_sessions(12);
        let tiny_frame = render(3, 100, &ws);
        assert!(
            tiny_frame
                .iter()
                .map(|line| strip(line))
                .any(|line| line.contains("> session-00"))
        );
        for expected in (0..12)
            .map(|index| format!("session-{index:02}"))
            .chain(std::iter::once("root".to_string()))
        {
            let frame = render(8, 100, &ws);
            let selected = frame
                .iter()
                .map(|line| strip(line))
                .find(|line| line.trim_start().starts_with('>'))
                .expect("selected row must be inside the viewport");
            assert!(selected.contains(&expected), "selected row: {selected}");
            if expected == "root" {
                let text = frame
                    .iter()
                    .map(|line| strip(line))
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(!text.contains("session-00"));
            }
            ws.select_next();
        }
    }

    #[test]
    fn render_fills_the_terminal_and_fits_its_width() {
        let frame = render(30, 100, &workspace());
        assert_eq!(frame.len(), 30);
        assert!(frame.iter().all(|line| display_width(line) == 100));
    }

    #[test]
    fn render_falls_back_for_a_zero_size() {
        let frame = render(0, 0, &workspace());
        assert_eq!(frame.len(), 24);
        assert!(frame.iter().all(|line| display_width(line) == 80));
    }

    #[test]
    fn render_does_not_overflow_a_short_terminal() {
        assert_eq!(render(2, 80, &workspace()).len(), 2);
        assert_eq!(render(1, 80, &workspace()).len(), 1);
    }
}

//! Welcome 画面（最初のトップメニュー）。
//!
//! 起動直後に出るメニュー画面。左に Open / New / Config / Quit のメニュー、右に最近使った
//! workspace（recent）のカードを 2 カラムで並べ、マスコットとタイトルを上に置く。状態
//! （[`Welcome`]）は端末 IO を持たず、[`render`] が状態を 1 フレーム分の行（ANSI 付き
//! `Vec<String>`）に変換する純粋関数である。色は [`crate::presentation::theme`] の意味的な
//! 役割で載せる。
//!
//! キー入力の解釈（どのキーをどの操作に写すか）は入力層が整うときに載せる。ここでは状態を
//! 動かすのに必要な純粋な操作 — 選択の上下移動（[`Welcome::select_next`] /
//! [`Welcome::select_prev`]）、選択中項目の確定（[`Welcome::selected_action`]）、
//! ショートカット文字→操作の写像（[`Welcome::action_for`]）— だけを公開する。

use chrono::{DateTime, Utc};

use usagi_core::domain::workspace::WorkspaceOverview;

use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::{self, icon, modal};

/// 画面上部に置くタイトル。
const TITLE: &str = "USAGI";
/// 左のメニュー列の固定表示幅。
const MENU_WIDTH: usize = 18;
/// 右の recent 列の固定表示幅。
const RECENT_WIDTH: usize = 32;
/// メニュー列と recent 列の間、区切り線の左右に置く余白。
const COLUMN_GAP: usize = 4;
/// recent カードの内側（内容）幅。枠線 2 桁＋左右余白 2 桁を引く。
const RECENT_INNER_WIDTH: usize = RECENT_WIDTH - 4;
/// 2 カラムブロック全体（メニュー列＋左右の余白＋区切り 1 桁＋recent 列）の表示幅。
/// これを 1 つの塊として端末に中央寄せすることで、幅の広い recent 列に引きずられて右へ
/// ずれることなく、マスコット・タイトルの真下に釣り合って収まる。
const MENU_BLOCK_WIDTH: usize = MENU_WIDTH + COLUMN_GAP + 1 + COLUMN_GAP + RECENT_WIDTH;

/// メニュー項目を選んだときにアプリへ返す操作。どの操作が何をするかはオーケストレータが決める。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    /// 既存 workspace を開く画面へ。
    Open,
    /// 新規 workspace 作成画面へ。
    New,
    /// 設定画面へ。
    Config,
    /// welcome 画面を離れて終了する。
    Quit,
    /// recent 一覧の `usize` 番目の workspace を開く。
    OpenRecent(usize),
}

/// 左メニューの 1 項目。ラベルと確定ショートカット文字を持つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuItem {
    /// 表示ラベル。
    pub label: &'static str,
    /// この項目を直接選ぶショートカット文字。
    pub key: char,
}

/// 右カラムに出す recent workspace の 1 枚のカード。数値は
/// [`WorkspaceOverview`] から取り込む。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentItem {
    /// workspace 名。
    pub label: String,
    /// この workspace を開く番号キー（`1`〜`3`）。
    pub key: char,
    /// 最終利用時刻。相対表記に使う。
    pub updated_at: DateTime<Utc>,
    /// 記録済みセッション数。
    pub session_count: usize,
    /// 未 done の issue 数。
    pub open_issue_count: usize,
    /// 検出済み PR 数。
    pub pr_count: usize,
}

/// welcome メニューの状態。端末 IO を持たず、[`render`] に渡して描画する。
#[derive(Debug, Clone)]
pub struct Welcome {
    items: Vec<MenuItem>,
    recent_items: Vec<RecentItem>,
    selected_index: usize,
    notice: Option<String>,
}

/// メニューの固定項目。ラベルと確定ショートカット文字の対応の単一情報源。
fn default_items() -> Vec<MenuItem> {
    vec![
        MenuItem {
            label: "Open",
            key: 'o',
        },
        MenuItem {
            label: "New",
            key: 'e',
        },
        MenuItem {
            label: "Config",
            key: 'c',
        },
        MenuItem {
            label: "Quit",
            key: 'q',
        },
    ]
}

/// recent 一覧の `index` 番目に割り当てる番号キー（`1` 始まり）。範囲外は `?`。
fn recent_key(index: usize) -> char {
    u32::try_from(index + 1)
        .ok()
        .and_then(|n| char::from_digit(n, 10))
        .unwrap_or('?')
}

/// ショートカット文字 `key` に対応する [`MenuItem`] の操作。メニュー項目のキーはすべて
/// 実在の操作へ写るので `Option` にせず、確定に使える。
fn activate(key: char) -> MenuAction {
    match key {
        'o' => MenuAction::Open,
        'e' => MenuAction::New,
        'c' => MenuAction::Config,
        // 残るメニューキーは 'q' だけ。呼び出し側は実在する項目のキーしか渡さない。
        _ => MenuAction::Quit,
    }
}

impl Welcome {
    /// recent workspace（最近使った順）を添えてメニューを組む。recent は先頭 3 件だけ取り、
    /// `1`〜`3` の番号キーを振る。
    #[must_use]
    pub fn new(recent: Vec<WorkspaceOverview>) -> Self {
        let recent_items = recent
            .into_iter()
            .take(3)
            .enumerate()
            .map(|(index, overview)| RecentItem {
                label: overview.workspace.name,
                key: recent_key(index),
                updated_at: overview.workspace.updated_at,
                session_count: overview.session_count,
                open_issue_count: overview.open_issue_count,
                pr_count: overview.pr_count,
            })
            .collect();
        Self {
            items: default_items(),
            recent_items,
            selected_index: 0,
            notice: None,
        }
    }

    /// recent workspace を持たないメニュー。
    #[must_use]
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// メニュー項目。
    #[must_use]
    pub fn items(&self) -> &[MenuItem] {
        &self.items
    }

    /// recent カード。
    #[must_use]
    pub fn recent_items(&self) -> &[RecentItem] {
        &self.recent_items
    }

    /// 選択中の項目の添字。
    #[must_use]
    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    /// 一時的な通知（サブ画面から戻ったときのメッセージなど）。
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// 選択を 1 つ下へ（末尾から先頭へ回り込む）。移動で通知は消える。
    pub fn select_next(&mut self) {
        self.selected_index = (self.selected_index + 1) % self.items.len();
        self.notice = None;
    }

    /// 選択を 1 つ上へ（先頭から末尾へ回り込む）。移動で通知は消える。
    pub fn select_prev(&mut self) {
        self.selected_index = self
            .selected_index
            .checked_sub(1)
            .unwrap_or(self.items.len().saturating_sub(1));
        self.notice = None;
    }

    /// 通知を差し替える（サブ画面から戻ったときに使う）。
    pub fn set_notice(&mut self, notice: Option<String>) {
        self.notice = notice;
    }

    /// 選択中の項目を確定したときの操作。
    #[must_use]
    pub fn selected_action(&self) -> MenuAction {
        activate(self.items[self.selected_index].key)
    }

    /// ショートカット文字 `key` に対応する操作。メニュー項目のキー（`o`/`e`/`c`/`q`）か、
    /// recent の番号キー（`1`〜`3`）に一致すればその操作、どれでもなければ `None`。
    #[must_use]
    pub fn action_for(&self, key: char) -> Option<MenuAction> {
        if let Some(item) = self.items.iter().find(|item| item.key == key) {
            return Some(activate(item.key));
        }
        self.recent_action(key)
    }

    /// 番号キー `key` に対応する recent workspace を開く操作。空スロットや範囲外は `None`。
    fn recent_action(&self, key: char) -> Option<MenuAction> {
        let digit = usize::try_from(key.to_digit(10)?).ok()?;
        if (1..=self.recent_items.len()).contains(&digit) {
            Some(MenuAction::OpenRecent(digit - 1))
        } else {
            None
        }
    }
}

impl Default for Welcome {
    fn default() -> Self {
        Self::empty()
    }
}

/// `text` を幅 `width` に中央寄せし、`style` で塗った 1 行。端末より広いテキストは
/// [`widgets::clip_to_width`] で省略記号付きに切ってから寄せる。
fn centered_line(width: usize, text: &str, style: Style) -> String {
    let clipped = widgets::clip_to_width(text, width);
    let pad = widgets::centered_padding(width, widgets::display_width(&clipped));
    format!("{}{}", " ".repeat(pad), style.paint(&clipped))
}

/// マスコット＋タイトルのヘッダ行。垂直位置は [`render`] が決めるので先頭余白は付けない。
fn header_lines(width: usize) -> Vec<String> {
    let mut lines = icon::centered(width);
    lines.push(String::new());
    lines.push(centered_line(width, TITLE, Role::Success.style().bold()));
    lines
}

/// 左のメニュー列。選択中の項目を強調する。
fn menu_column_lines(items: &[MenuItem], selected_index: usize) -> Vec<String> {
    // "> Label...... key" — カーソル + 10 桁ラベル + 右寄せキー。
    let mut lines = vec![Role::Success.style().bold().paint("Menu"), String::new()];
    for (i, item) in items.iter().enumerate() {
        let is_selected = i == selected_index;
        let cursor = if is_selected {
            Role::Danger.style().bold().paint(">")
        } else {
            " ".to_string()
        };
        let label_text = format!("{:<10}", item.label);
        let label = if is_selected {
            Role::Accent.style().bold().paint(&label_text)
        } else {
            label_text
        };
        let key_text = format!("{:>5}", item.key);
        let key = if is_selected {
            Role::Warning.style().paint(&key_text)
        } else {
            key_text
        };
        lines.push(format!("{cursor} {label} {key}"));
        lines.push(String::new());
    }
    lines
}

/// recent カード 1 枚。実データがなければ番号付きのプレースホルダを描く。
fn recent_card(item: Option<&RecentItem>, index: usize, now: DateTime<Utc>) -> Vec<String> {
    match item {
        Some(item) => {
            let title = format!(
                "{} {}",
                Role::Warning.style().bold().paint(&item.key.to_string()),
                item.label
            );
            let body = vec![Style::new().dim().paint(&format!(
                "◷ {}  ⎇ {}  #{}  ● {}",
                widgets::relative_time(item.updated_at, now),
                item.session_count,
                item.pr_count,
                item.open_issue_count
            ))];
            modal::boxed(&title, RECENT_INNER_WIDTH, &body)
        }
        None => modal::boxed(
            &format!("{} —", recent_key(index)),
            RECENT_INNER_WIDTH,
            &[Style::new().dim().paint("No recent workspace")],
        ),
    }
}

/// 右カラムの recent 群。見出し＋常に 3 枚のカードで高さを固定し、読み込みや空でも
/// マスコット行がずれないようにする。
fn recent_lines(items: &[RecentItem], now: DateTime<Utc>) -> Vec<String> {
    let mut lines = vec![Role::Success.style().bold().paint("Recent")];
    for i in 0..3 {
        lines.extend(recent_card(items.get(i), i, now));
    }
    lines
}

/// ANSI 付きの断片を表示幅 `width` に詰める（広ければ切り、狭ければ空白で右を埋める）ので、
/// 次のカラムが安定した位置から始まる。
fn pad_segment(segment: &str, width: usize) -> String {
    let clipped = widgets::clip_to_width(segment, width);
    let visible = widgets::display_width(&clipped);
    format!("{clipped}{}", " ".repeat(width.saturating_sub(visible)))
}

/// メニュー列（左）と recent 群（右）を区切り線で結んだ 2 カラムブロック。ブロック全体を
/// 端末に中央寄せする。
fn menu_lines(
    width: usize,
    items: &[MenuItem],
    selected_index: usize,
    recent_items: &[RecentItem],
    now: DateTime<Utc>,
) -> Vec<String> {
    let left = menu_column_lines(items, selected_index);
    let right = recent_lines(recent_items, now);
    let row_count = left.len().max(right.len());
    let left_pad = " ".repeat(widgets::centered_padding(width, MENU_BLOCK_WIDTH));
    let gap = " ".repeat(COLUMN_GAP);
    let divider = Style::new().dim().paint("│");

    (0..row_count)
        .map(|i| {
            let left = pad_segment(left.get(i).map_or("", String::as_str), MENU_WIDTH);
            let right = pad_segment(right.get(i).map_or("", String::as_str), RECENT_WIDTH);
            let line = format!("{left_pad}{left}{gap}{divider}{gap}{right}");
            widgets::clip_to_width(&line, width)
        })
        .collect()
}

/// メニュー下の一時通知行。通知が無くても必ず 1 行（空行）返し、出現・消滅でレイアウトが
/// ずれないようにする。
fn notice_lines(width: usize, notice: Option<&str>) -> Vec<String> {
    match notice {
        None => vec![String::new()],
        Some(text) => vec![centered_line(width, text, Role::Warning.style())],
    }
}

/// 画面下部に固定するキー操作ヒント。[`render`] が最下行に貼り付ける。
fn footer_lines(width: usize) -> Vec<String> {
    let footer = "↑↓: move / Enter or letter: select / 1-3: recent / q: quit";
    vec![centered_line(width, footer, Style::new().dim())]
}

/// `body_lines` 行のボディを、`footer_lines` 行のフッタの上で `height` 行に垂直中央寄せ
/// するときの上の空行数。
fn centered_top_padding(height: usize, body_lines: usize, footer_lines: usize) -> usize {
    height.saturating_sub(body_lines + footer_lines) / 2
}

/// 生の端末サイズ `raw_height`×`raw_width` に対する welcome 画面 1 フレーム分の行。
/// ボディ（マスコット・タイトル・2 カラムメニュー・通知）は垂直中央寄せ、フッタは最下行に
/// 固定する。サイズ 0 は 80×24 にフォールバックする。`now` は recent の相対時刻に使うので
/// 呼び出し側が渡す（この層は実時計を読まない）。
#[must_use]
pub fn render(
    raw_height: usize,
    raw_width: usize,
    welcome: &Welcome,
    now: DateTime<Utc>,
) -> Vec<String> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);

    let mut body = header_lines(width);
    // 中央寄せしたタイトルと 2 カラムの区切り線の間に 1 行の余白を置く。
    body.push(String::new());
    body.extend(menu_lines(
        width,
        welcome.items(),
        welcome.selected_index(),
        welcome.recent_items(),
        now,
    ));
    body.extend(notice_lines(width, welcome.notice()));
    let footer = footer_lines(width);

    let mut lines = Vec::with_capacity(height);
    let top_padding = centered_top_padding(height, body.len(), footer.len());
    for _ in 0..top_padding {
        lines.push(String::new());
    }
    lines.extend(body);

    // フッタを最下行まで押し下げる。
    let bottom_padding = height.saturating_sub(lines.len() + footer.len());
    for _ in 0..bottom_padding {
        lines.push(String::new());
    }
    lines.extend(footer);
    lines
}

#[cfg(test)]
mod tests {
    use super::{MenuAction, Welcome, render};
    use crate::presentation::widgets::display_width;
    use chrono::{DateTime, Duration, Utc};
    use usagi_core::domain::workspace::{Workspace, WorkspaceOverview};

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-06-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn overview(name: &str, minutes_ago: i64) -> WorkspaceOverview {
        let mut workspace = Workspace::new(name, format!("/tmp/{name}"));
        workspace.updated_at = now() - Duration::minutes(minutes_ago);
        WorkspaceOverview::new(workspace, 2, 4, 1)
    }

    fn strip(line: &str) -> String {
        // ANSI SGR を落として素のテキストにする（表示内容の検証用）。
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

    #[test]
    fn new_welcome_starts_at_the_first_item_without_a_notice() {
        let welcome = Welcome::empty();
        assert_eq!(welcome.selected_index(), 0);
        assert_eq!(welcome.notice(), None);
        assert_eq!(welcome.items().len(), 4);
        assert!(welcome.recent_items().is_empty());
        // derive された Clone / Debug も計測対象なのでここで触れる。
        assert!(format!("{:?}", welcome.clone()).contains("Welcome"));
    }

    #[test]
    fn default_matches_empty() {
        assert_eq!(Welcome::default().selected_index(), 0);
        assert!(Welcome::default().recent_items().is_empty());
    }

    #[test]
    fn select_next_advances_and_wraps() {
        let mut welcome = Welcome::empty();
        welcome.select_next();
        assert_eq!(welcome.selected_index(), 1);
        welcome.select_next(); // New -> Config
        welcome.select_next(); // Config -> Quit
        assert_eq!(welcome.selected_index(), 3);
        welcome.select_next(); // Quit -> wrap to Open
        assert_eq!(welcome.selected_index(), 0);
    }

    #[test]
    fn select_prev_wraps_to_the_last_item() {
        let mut welcome = Welcome::empty();
        welcome.select_prev();
        assert_eq!(welcome.selected_index(), 3);
        welcome.select_prev();
        assert_eq!(welcome.selected_index(), 2);
    }

    #[test]
    fn movement_clears_an_existing_notice() {
        let mut welcome = Welcome::empty();
        welcome.set_notice(Some("saved".to_string()));
        welcome.select_next();
        assert_eq!(welcome.notice(), None);
        welcome.set_notice(Some("saved".to_string()));
        welcome.select_prev();
        assert_eq!(welcome.notice(), None);
    }

    #[test]
    fn set_notice_replaces_the_notice() {
        let mut welcome = Welcome::empty();
        welcome.set_notice(Some("done".to_string()));
        assert_eq!(welcome.notice(), Some("done"));
        welcome.set_notice(None);
        assert_eq!(welcome.notice(), None);
    }

    #[test]
    fn selected_action_reports_the_current_item() {
        let mut welcome = Welcome::empty();
        assert_eq!(welcome.selected_action(), MenuAction::Open);
        welcome.select_next();
        assert_eq!(welcome.selected_action(), MenuAction::New);
        welcome.select_next();
        assert_eq!(welcome.selected_action(), MenuAction::Config);
        welcome.select_next();
        assert_eq!(welcome.selected_action(), MenuAction::Quit);
    }

    #[test]
    fn action_for_maps_menu_shortcuts() {
        let welcome = Welcome::empty();
        assert_eq!(welcome.action_for('o'), Some(MenuAction::Open));
        assert_eq!(welcome.action_for('e'), Some(MenuAction::New));
        assert_eq!(welcome.action_for('c'), Some(MenuAction::Config));
        assert_eq!(welcome.action_for('q'), Some(MenuAction::Quit));
        // 未知のキーは None。
        assert_eq!(welcome.action_for('z'), None);
    }

    #[test]
    fn action_for_maps_recent_number_keys() {
        let welcome = Welcome::new(vec![overview("alpha", 11), overview("beta", 180)]);
        assert_eq!(welcome.action_for('1'), Some(MenuAction::OpenRecent(0)));
        assert_eq!(welcome.action_for('2'), Some(MenuAction::OpenRecent(1)));
        // 空スロット（3 件未満）や範囲外は None。
        assert_eq!(welcome.action_for('3'), None);
        assert_eq!(welcome.action_for('0'), None);
    }

    #[test]
    fn recent_items_are_limited_to_three_and_numbered() {
        let welcome = Welcome::new(vec![
            overview("alpha", 1),
            overview("beta", 2),
            overview("gamma", 3),
            overview("delta", 4),
        ]);
        assert_eq!(welcome.recent_items().len(), 3);
        assert_eq!(welcome.recent_items()[0].label, "alpha");
        assert_eq!(welcome.recent_items()[0].key, '1');
        assert_eq!(welcome.recent_items()[2].label, "gamma");
        assert_eq!(welcome.recent_items()[2].key, '3');
        // derive された Clone / PartialEq / Debug（RecentItem）を触れる。
        assert_eq!(welcome.recent_items()[0].clone(), welcome.recent_items()[0]);
        assert!(format!("{:?}", welcome.recent_items()[0]).contains("alpha"));
        // MenuItem の derive も。
        assert_eq!(welcome.items()[0], welcome.items()[0]);
        assert!(format!("{:?}", welcome.items()[0]).contains("Open"));
        // MenuAction の derive も。
        assert_eq!(MenuAction::OpenRecent(0), MenuAction::OpenRecent(0));
        assert!(format!("{:?}", MenuAction::Open).contains("Open"));
    }

    #[test]
    fn render_combines_every_section() {
        let welcome = Welcome::new(vec![overview("alpha", 11)]);
        let frame = render(0, 0, &welcome, now());
        let joined: String = frame
            .iter()
            .map(|l| strip(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("USAGI"));
        assert!(joined.contains("Menu"));
        assert!(joined.contains("Recent"));
        assert!(joined.contains("Open"));
        assert!(joined.contains("Quit"));
        assert!(joined.contains("alpha"));
        // recent カードの数値と相対時刻。
        assert!(joined.contains("11min ago"));
        assert!(joined.contains("#1"));
        assert!(joined.contains("● 4"));
        assert!(joined.contains('│'));
        // フッタのヒント。
        assert!(joined.contains("q: quit"));
    }

    #[test]
    fn render_marks_only_the_selected_row() {
        let mut welcome = Welcome::empty();
        welcome.select_next(); // New を選択
        let frame = render(24, 80, &welcome, now());
        // カーソル ">" はちょうど 1 行に出る。
        assert_eq!(frame.iter().filter(|l| strip(l).contains('>')).count(), 1);
    }

    #[test]
    fn render_shows_a_placeholder_when_there_are_no_recents() {
        let welcome = Welcome::empty();
        let frame = render(24, 80, &welcome, now());
        let joined: String = frame
            .iter()
            .map(|l| strip(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("No recent workspace"));
        assert!(joined.contains("1 —"));
    }

    #[test]
    fn render_renders_the_notice_line() {
        let mut welcome = Welcome::empty();
        welcome.set_notice(Some("welcome back".to_string()));
        let frame = render(24, 80, &welcome, now());
        let joined: String = frame
            .iter()
            .map(|l| strip(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("welcome back"));
    }

    #[test]
    fn render_centers_the_body_and_pins_the_footer() {
        let welcome = Welcome::empty();
        let height = 40;
        let frame = render(height, 80, &welcome, now());
        // フレームはちょうど端末の高さを満たす。
        assert_eq!(frame.len(), height);
        // フッタは最下行。
        assert!(strip(frame.last().unwrap()).contains("q: quit"));
        // 先頭の空行がボディを垂直中央寄せする。
        let top_padding = frame.iter().take_while(|l| l.is_empty()).count();
        assert!(top_padding > 0);
        assert!(!frame[top_padding].is_empty());
    }

    #[test]
    fn render_keeps_the_notice_slot_stable_across_toggling() {
        let mut welcome = Welcome::empty();
        let without = render(24, 80, &welcome, now());
        welcome.set_notice(Some("Config is coming soon".to_string()));
        let with = render(24, 80, &welcome, now());
        // 通知の有無でフレーム高は変わらない（メニューやフッタが動かない）。
        assert_eq!(without.len(), with.len());
    }

    #[test]
    fn render_does_not_lose_content_on_a_short_terminal() {
        let welcome = Welcome::empty();
        let frame = render(3, 80, &welcome, now());
        let joined: String = frame
            .iter()
            .map(|l| strip(l))
            .collect::<Vec<_>>()
            .join("\n");
        // 中央寄せの余白は 0 に飽和し、内容は切り落とされない。
        assert!(joined.contains("USAGI"));
        assert!(strip(frame.last().unwrap()).contains("q: quit"));
    }

    #[test]
    fn render_rows_fit_the_terminal_width() {
        let welcome = Welcome::new(vec![overview("alpha", 11)]);
        let frame = render(24, 80, &welcome, now());
        // どの行も端末幅を超えない（2 カラムブロックが幅内に収まる）。
        assert!(frame.iter().all(|l| display_width(l) <= 80));
    }
}

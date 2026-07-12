#![coverage(off)]

//! Welcome 画面（最初のトップメニュー）。
//!
//! 起動直後に出るメニュー画面。左に Open / New / Config / Quit のメニュー、右に最近使った
//! 項目（recent）のカードを 2 カラムで並べ、マスコットとタイトルを上に置く。recent の各項目は
//! 単体 workspace か、一緒に開いた workspace の合併（unite）のどちらかで、カードの見た目を
//! 変えて描き分ける。状態（[`Welcome`]）は端末 IO を持たず、[`render`] が状態を 1 フレーム分の
//! 行（ANSI 付き `Vec<String>`）に変換する純粋関数である。色は
//! [`crate::presentation::theme`] の意味的な役割で載せる。
//!
//! キー入力の解釈（どのキーをどの操作に写すか）は入力層が整うときに載せる。ここでは状態を
//! 動かすのに必要な純粋な操作 — 選択の上下移動（[`Welcome::select_next`] /
//! [`Welcome::select_prev`]）、選択中項目の確定（[`Welcome::selected_action`]）、
//! ショートカット文字→操作の写像（[`Welcome::action_for`]）— だけを公開する。

use chrono::{DateTime, Utc};

use usagi_core::domain::recent::Recent;
use usagi_core::domain::workspace::Workspace;

use crate::presentation::layouts::mascot_screen;
use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::{self, modal};

/// 画面上部に置くタイトル。
const TITLE: &str = "USAGI";
/// 最下行に固定するキー操作ヒント。
const FOOTER: &str = "↑↓/jk: move / Enter or letter: select / 1-3: recent / q: quit";
/// 左のメニュー列の固定表示幅。
const MENU_WIDTH: usize = 18;
/// 右の recent 列の固定表示幅。
const RECENT_WIDTH: usize = 32;
/// recent 列に出すカードの枚数（キー `1`〜`3`）。
const RECENT_SLOTS: usize = 3;
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
    /// recent 一覧の `usize` 番目の項目を開く。
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

/// welcome メニューの状態。端末 IO を持たず、[`render`] に渡して描画する。recent は
/// [`Recent`] の配列で持ち、単体 workspace と unite を同じリストに混ぜて表示する。
#[derive(Debug, Clone)]
pub struct Welcome {
    items: Vec<MenuItem>,
    recent: Vec<Recent>,
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
    /// recent 項目（最近使った順）を添えてメニューを組む。表示と番号キーの対象は
    /// 先頭 [`RECENT_SLOTS`] 件だけだが、後で touch された項目を再整列できるよう全件を保持する。
    #[must_use]
    pub fn new(recent: Vec<Recent>) -> Self {
        Self {
            items: default_items(),
            recent,
            selected_index: 0,
            notice: None,
        }
    }

    /// recent 項目を持たないメニュー。
    #[must_use]
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// メニュー項目。
    #[must_use]
    pub fn items(&self) -> &[MenuItem] {
        &self.items
    }

    /// recent 項目（単体 workspace / unite が混在する）。
    #[must_use]
    pub fn recent(&self) -> &[Recent] {
        &self.recent[..self.recent.len().min(RECENT_SLOTS)]
    }

    /// `workspace` と同じ path の単体 recent に touch 後の identity / timestamp を反映し、
    /// 最終利用時刻の降順へ戻す。overview の集計値は既存 model の値を保つ。
    pub(crate) fn record_opened(&mut self, workspace: &Workspace) {
        let Some(overview) = self.recent.iter_mut().find_map(|recent| match recent {
            Recent::Workspace(overview) if overview.workspace.path == workspace.path => {
                Some(overview)
            }
            Recent::Workspace(_) | Recent::Unite(_) => None,
        }) else {
            return;
        };
        overview.workspace = workspace.clone();
        self.recent
            .sort_by_key(|recent| std::cmp::Reverse(recent.updated_at()));
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

    /// 番号キー `key` に対応する recent 項目を開く操作。空スロットや範囲外は `None`。
    fn recent_action(&self, key: char) -> Option<MenuAction> {
        let digit = usize::try_from(key.to_digit(10)?).ok()?;
        if (1..=self.recent().len()).contains(&digit) {
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

/// recent カードのタイトルに置く番号キー（Warning 太字）。
fn card_key(key: char) -> String {
    Role::Warning.style().bold().paint(&key.to_string())
}

/// recent カードの本文行（dim）。
fn card_body_line(text: &str) -> String {
    Style::new().dim().paint(text)
}

/// カウント行 `◷ 相対時刻  ⎇ セッション  #PR  ● 未 done issue`。単体・unite 共通の書式。
fn counts_line(relative: &str, sessions: usize, prs: usize, open_issues: usize) -> String {
    card_body_line(&format!(
        "◷ {relative}  ⎇ {sessions}  #{prs}  ● {open_issues}"
    ))
}

/// recent カード 1 枚。[`Recent`] のバリアントで描き分ける。実データが無いスロットは番号付きの
/// プレースホルダを描く。
///
/// - 単体 workspace: タイトル `key name`、本文はカウント 1 行。
/// - unite（合併）: タイトル `key primary +追加数`、本文はメンバー名（`∪`）＋合計カウントの 2 行。
fn recent_card(item: Option<&Recent>, index: usize, now: DateTime<Utc>) -> Vec<String> {
    let key = recent_key(index);
    match item {
        Some(Recent::Workspace(overview)) => {
            let title = format!("{} {}", card_key(key), overview.workspace.name);
            let body = vec![counts_line(
                &widgets::relative_time(overview.workspace.updated_at, now),
                overview.session_count,
                overview.pr_count,
                overview.open_issue_count,
            )];
            modal::boxed(&title, RECENT_INNER_WIDTH, &body)
        }
        Some(Recent::Unite(unite)) => {
            let title = format!(
                "{} {} +{}",
                card_key(key),
                unite.primary_name(),
                unite.extra_count()
            );
            let names = unite
                .members()
                .iter()
                .map(|member| member.workspace.name.as_str())
                .collect::<Vec<_>>()
                .join(" · ");
            let relative = unite
                .updated_at()
                .map_or_else(|| "—".to_string(), |at| widgets::relative_time(at, now));
            let body = vec![
                card_body_line(&format!("∪ {names}")),
                counts_line(
                    &relative,
                    unite.session_count(),
                    unite.pr_count(),
                    unite.open_issue_count(),
                ),
            ];
            modal::boxed(&title, RECENT_INNER_WIDTH, &body)
        }
        None => modal::boxed(
            &format!("{key} —"),
            RECENT_INNER_WIDTH,
            &[card_body_line("No recent workspace")],
        ),
    }
}

/// 右カラムの recent 群。見出し＋常に [`RECENT_SLOTS`] 枚のカードを出し、読み込みや空でも
/// 列の見出し位置がずれないようにする。
fn recent_lines(items: &[Recent], now: DateTime<Utc>) -> Vec<String> {
    let mut lines = vec![Role::Success.style().bold().paint("Recent")];
    for i in 0..RECENT_SLOTS {
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
/// 端末に中央寄せする。unite カードは行数が多いので、片方の列が尽きた行は空白で埋める。
fn menu_lines(
    width: usize,
    items: &[MenuItem],
    selected_index: usize,
    recent: &[Recent],
    now: DateTime<Utc>,
) -> Vec<String> {
    let left = menu_column_lines(items, selected_index);
    let right = recent_lines(recent, now);
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
        Some(text) => vec![mascot_screen::centered_line(
            width,
            text,
            Role::Warning.style(),
        )],
    }
}

/// 生の端末サイズ `raw_height`×`raw_width` に対する welcome 画面 1 フレーム分の行。
/// マスコット・タイトル・フッタの配置は共通の [`mascot_screen`] レイアウトに任せ、この関数は
/// ボディ（2 カラムメニュー＋通知）だけを組む。`now` は recent の相対時刻に使うので呼び出し側が
/// 渡す（この層は実時計を読まない）。
#[must_use]
pub fn render(
    raw_height: usize,
    raw_width: usize,
    welcome: &Welcome,
    now: DateTime<Utc>,
) -> Vec<String> {
    mascot_screen::render(raw_height, raw_width, TITLE, FOOTER, |width| {
        let mut body = menu_lines(
            width,
            welcome.items(),
            welcome.selected_index(),
            welcome.recent(),
            now,
        );
        body.extend(notice_lines(width, welcome.notice()));
        body
    })
}

#[cfg(test)]
mod tests {
    use super::{MenuAction, Welcome, render};
    use crate::presentation::widgets::display_width;
    use chrono::{DateTime, Duration, Utc};
    use usagi_core::domain::recent::{Recent, UniteOverview};
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

    /// 単体 workspace の recent 項目。
    fn workspace(name: &str, minutes_ago: i64) -> Recent {
        Recent::Workspace(overview(name, minutes_ago))
    }

    /// 与えた (名前, 何分前) のメンバーからなる unite の recent 項目。
    fn unite(members: &[(&str, i64)]) -> Recent {
        Recent::Unite(UniteOverview::new(
            members
                .iter()
                .map(|(name, minutes)| overview(name, *minutes))
                .collect(),
        ))
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

    fn rendered(welcome: &Welcome) -> String {
        render(24, 80, welcome, now())
            .iter()
            .map(|l| strip(l))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn new_welcome_starts_at_the_first_item_without_a_notice() {
        let welcome = Welcome::empty();
        assert_eq!(welcome.selected_index(), 0);
        assert_eq!(welcome.notice(), None);
        assert_eq!(welcome.items().len(), 4);
        assert!(welcome.recent().is_empty());
        // derive された Clone / Debug も計測対象なのでここで触れる。
        assert!(format!("{:?}", welcome.clone()).contains("Welcome"));
    }

    #[test]
    fn default_matches_empty() {
        assert_eq!(Welcome::default().selected_index(), 0);
        assert!(Welcome::default().recent().is_empty());
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
        let welcome = Welcome::new(vec![
            workspace("alpha", 11),
            unite(&[("beta", 180), ("gamma", 5)]),
        ]);
        assert_eq!(welcome.action_for('1'), Some(MenuAction::OpenRecent(0)));
        assert_eq!(welcome.action_for('2'), Some(MenuAction::OpenRecent(1)));
        // 空スロット（3 件未満）や範囲外は None。
        assert_eq!(welcome.action_for('3'), None);
        assert_eq!(welcome.action_for('0'), None);
    }

    #[test]
    fn recent_is_limited_to_three() {
        let welcome = Welcome::new(vec![
            workspace("alpha", 1),
            workspace("beta", 2),
            workspace("gamma", 3),
            workspace("delta", 4),
        ]);
        assert_eq!(welcome.recent().len(), 3);
        assert_eq!(welcome.action_for('4'), None);
        // MenuItem / MenuAction の derive も計測対象なのでここで触れる。
        assert_eq!(welcome.items()[0], welcome.items()[0]);
        assert!(format!("{:?}", welcome.items()[0]).contains("Open"));
        assert_eq!(MenuAction::OpenRecent(0), MenuAction::OpenRecent(0));
        assert!(format!("{:?}", MenuAction::Open).contains("Open"));
    }

    #[test]
    fn record_opened_promotes_a_hidden_recent_and_preserves_its_counts() {
        let mut welcome = Welcome::new(vec![
            workspace("alpha", 1),
            unite(&[("pair-a", 2), ("pair-b", 3)]),
            workspace("beta", 3),
            workspace("gamma", 4),
            workspace("delta", 5),
        ]);
        let mut touched = Workspace::new("delta", "/tmp/delta");
        touched.updated_at = now();

        welcome.record_opened(&touched);

        assert_eq!(welcome.recent().len(), 3);
        assert_eq!(
            welcome.recent()[0],
            Recent::Workspace(WorkspaceOverview::new(touched, 2, 4, 1))
        );
        let names = welcome
            .recent()
            .iter()
            .map(|recent| match recent {
                Recent::Workspace(overview) => overview.workspace.name.as_str(),
                Recent::Unite(_) => "unite",
            })
            .collect::<Vec<_>>();
        assert_eq!(names, ["delta", "alpha", "unite"]);
    }

    #[test]
    fn render_combines_every_section() {
        let welcome = Welcome::new(vec![workspace("alpha", 11)]);
        let joined = rendered(&welcome);
        assert!(joined.contains("USAGI"));
        assert!(joined.contains("Menu"));
        assert!(joined.contains("Recent"));
        assert!(joined.contains("Open"));
        assert!(joined.contains("Quit"));
        assert!(joined.contains("alpha"));
        // 単体 workspace カードのカウントと相対時刻。
        assert!(joined.contains("11min ago"));
        assert!(joined.contains("#1"));
        assert!(joined.contains("● 4"));
        assert!(joined.contains('│'));
        // フッタのヒント。
        assert!(joined.contains("q: quit"));
    }

    #[test]
    fn render_distinguishes_unite_cards_from_workspace_cards() {
        let welcome = Welcome::new(vec![
            workspace("solo", 30),
            unite(&[("alpha", 45), ("beta", 8)]),
        ]);
        let joined = rendered(&welcome);
        // 単体カード。
        assert!(joined.contains("solo"));
        // unite カード: primary +追加数、メンバー名（∪）、合計カウント（セッション 2+2=4）、
        // 最新メンバーの相対時刻（beta が 8min ago）。
        assert!(joined.contains("alpha +1"));
        assert!(joined.contains("∪ alpha · beta"));
        assert!(joined.contains("⎇ 4"));
        assert!(joined.contains("8min ago"));
    }

    #[test]
    fn render_handles_an_empty_unite() {
        // 退化した空 unite でも落ちず、相対時刻を "—" で表す。
        let welcome = Welcome::new(vec![unite(&[])]);
        let joined = rendered(&welcome);
        assert!(joined.contains('∪'));
        assert!(joined.contains("+0"));
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
        let joined = rendered(&Welcome::empty());
        assert!(joined.contains("No recent workspace"));
        assert!(joined.contains("1 —"));
    }

    #[test]
    fn render_renders_the_notice_line() {
        let mut welcome = Welcome::empty();
        welcome.set_notice(Some("welcome back".to_string()));
        let joined = rendered(&welcome);
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
        let welcome = Welcome::new(vec![
            workspace("alpha", 11),
            unite(&[("beta", 20), ("gamma", 5)]),
        ]);
        let frame = render(24, 80, &welcome, now());
        // どの行も端末幅を超えない（2 カラムブロックが幅内に収まる）。
        assert!(frame.iter().all(|l| display_width(l) <= 80));
    }
}

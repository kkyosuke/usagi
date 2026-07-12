#![coverage(off)]

//! Open 画面（登録済み workspace を開く）。
//!
//! welcome の Open から開く画面。usagi に登録済みの workspace を一覧で並べ、選んで開く。
//! 各行は workspace 名と最終利用の相対時刻を出し、リストの下に選択中の絶対パスを添える。
//! 状態（[`Open`]）は端末 IO を持たない純粋な値で、[`render`] が 1 フレーム分の行
//! （ANSI 付き `Vec<String>`）に変換する。マスコット・タイトル・フッタの配置は共通の
//! [`mascot_screen`] レイアウトに任せ、この view はボディ（一覧＋選択パス）だけを組む。
//!
//! 表示する workspace は永続化ストア（[`usagi_core::infrastructure::store::workspace`]）が
//! 持つが、その読み出しは実 IO なので合成ルートが行い、この層は受け取った一覧を描くだけである。
//! `now` は相対時刻に使うので呼び出し側が渡す（この層は実時計を読まない）。

use std::collections::HashSet;

use chrono::{DateTime, Utc};

use usagi_core::domain::workspace::Workspace;

use crate::presentation::layouts::mascot_screen;
use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets;

/// 画面上部に置くタイトル。
const TITLE: &str = "Open Workspace";
/// 最下行に固定するキー操作ヒント。
const FOOTER: &str = "↑↓/jk move / / filter / u Unite / c cleanup / Enter open / Esc back / q quit";
/// 一覧ブロック全体の表示幅。各行をこの幅の列に収めて桁を揃え、端末に中央寄せする。
const BLOCK_WIDTH: usize = 52;
/// workspace 名に割り当てる固定表示幅（溢れは省略記号で切る）。
const NAME_WIDTH: usize = 24;

/// Open 画面の状態。登録済み workspace の一覧と選択位置を持つ。端末 IO は持たない。
#[derive(Debug, Clone)]
pub struct Open {
    workspaces: Vec<Workspace>,
    selected_index: usize,
    filter: String,
    filtering: bool,
    unite: bool,
    unite_paths: HashSet<std::path::PathBuf>,
    cleanup_confirming: bool,
}

impl Open {
    /// workspace 一覧（登録順）からメニューを組む。
    #[must_use]
    pub fn new(workspaces: Vec<Workspace>) -> Self {
        Self {
            workspaces,
            selected_index: 0,
            filter: String::new(),
            filtering: false,
            unite: false,
            unite_paths: HashSet::new(),
            cleanup_confirming: false,
        }
    }

    /// 一覧の workspace。
    #[must_use]
    pub fn workspaces(&self) -> &[Workspace] {
        &self.workspaces
    }

    /// 選択中の項目の添字。
    #[must_use]
    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    /// 一覧が空か。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.workspaces.is_empty()
    }

    /// 選択中の workspace。空のときは `None`。
    #[must_use]
    pub fn selected(&self) -> Option<&Workspace> {
        self.filtered().get(self.selected_index).copied()
    }

    /// Whether filter text input owns printable keys.
    #[must_use]
    pub const fn filtering(&self) -> bool {
        self.filtering
    }

    /// The current case-insensitive workspace-name filter.
    #[must_use]
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Whether selection builds a Unite set rather than choosing one workspace.
    #[must_use]
    pub const fn is_unite(&self) -> bool {
        self.unite
    }

    /// Whether cleanup has been explicitly requested and awaits y/n confirmation.
    #[must_use]
    pub const fn cleanup_confirming(&self) -> bool {
        self.cleanup_confirming
    }

    /// Start accepting filter text.
    pub fn begin_filter(&mut self) {
        self.filtering = true;
    }

    /// Stop accepting filter text without discarding the filter.
    pub fn end_filter(&mut self) {
        self.filtering = false;
    }

    /// Append one character to the filter and return selection to its first hit.
    pub fn push_filter(&mut self, ch: char) {
        self.filter.push(ch);
        self.selected_index = 0;
    }

    /// Delete one filter character and return selection to its first hit.
    pub fn pop_filter(&mut self) {
        self.filter.pop();
        self.selected_index = 0;
    }

    /// Switch between Single and Unite selection. A new Unite set starts empty.
    pub fn toggle_unite(&mut self) {
        self.unite = !self.unite;
        self.unite_paths.clear();
    }

    /// Add or remove the selected workspace from the Unite set.
    pub fn toggle_unite_member(&mut self) {
        let Some(path) = self.selected().map(|workspace| workspace.path.clone()) else {
            return;
        };
        if !self.unite_paths.remove(&path) {
            self.unite_paths.insert(path);
        }
    }

    /// Return selected Unite paths in registry order.
    #[must_use]
    pub fn unite_paths(&self) -> Vec<std::path::PathBuf> {
        self.workspaces
            .iter()
            .filter(|workspace| self.unite_paths.contains(&workspace.path))
            .map(|workspace| workspace.path.clone())
            .collect()
    }

    /// Ask for an explicit cleanup confirmation.
    pub fn request_cleanup(&mut self) {
        self.cleanup_confirming = true;
    }

    /// Dismiss a cleanup confirmation without mutating the registry.
    pub fn cancel_cleanup(&mut self) {
        self.cleanup_confirming = false;
    }

    /// Finish a confirmed cleanup, removing the returned registry paths locally.
    pub fn remove_paths(&mut self, paths: &[std::path::PathBuf]) {
        self.workspaces
            .retain(|workspace| !paths.iter().any(|path| path == &workspace.path));
        self.unite_paths.retain(|path| !paths.contains(path));
        self.cleanup_confirming = false;
        self.selected_index = 0;
    }

    fn filtered(&self) -> Vec<&Workspace> {
        let filter = self.filter.to_lowercase();
        self.workspaces
            .iter()
            .filter(|workspace| workspace.name.to_lowercase().contains(&filter))
            .collect()
    }

    /// `workspace` と同じ path の項目に touch 後の値を反映し、最終利用時刻の降順へ
    /// 再整列する。順序が変わっても現在選択中の path を保つ。
    pub(crate) fn record_opened(&mut self, workspace: &Workspace) {
        let selected_path = self.selected().map(|selected| selected.path.clone());
        let Some(current) = self
            .workspaces
            .iter_mut()
            .find(|current| current.path == workspace.path)
        else {
            return;
        };
        *current = workspace.clone();
        self.workspaces
            .sort_by_key(|workspace| std::cmp::Reverse(workspace.updated_at));
        self.selected_index = selected_path
            .and_then(|selected_path| {
                self.filtered()
                    .iter()
                    .position(|current| current.path == selected_path)
            })
            .unwrap_or_default();
    }

    /// 選択を 1 つ下へ（末尾から先頭へ回り込む）。空一覧では何もしない。
    pub fn select_next(&mut self) {
        if self.filtered().is_empty() {
            return;
        }
        let len = self.filtered().len();
        if len == 0 {
            return;
        }
        self.selected_index = (self.selected_index + 1) % len;
    }

    /// 選択を 1 つ上へ（先頭から末尾へ回り込む）。空一覧では何もしない。
    pub fn select_prev(&mut self) {
        if self.filtered().is_empty() {
            return;
        }
        let len = self.filtered().len();
        if len == 0 {
            return;
        }
        self.selected_index = self.selected_index.checked_sub(1).unwrap_or(len - 1);
    }
}

/// ANSI 付き断片を表示幅 `width` に詰める（広ければ切り、狭ければ空白で右を埋める）。
fn fit(text: &str, width: usize) -> String {
    let clipped = widgets::clip_to_width(text, width);
    let visible = widgets::display_width(&clipped);
    format!("{clipped}{}", " ".repeat(width.saturating_sub(visible)))
}

/// 一覧の 1 行 `> name...... 3d ago`。選択行はカーソルと名前を強調する。
fn workspace_row(workspace: &Workspace, is_selected: bool, now: DateTime<Utc>) -> String {
    let cursor = if is_selected {
        Role::Danger.style().bold().paint(">")
    } else {
        " ".to_string()
    };
    let name = fit(&workspace.name, NAME_WIDTH);
    let name = if is_selected {
        Role::Accent.style().bold().paint(&name)
    } else {
        name
    };
    let relative = Style::new()
        .dim()
        .paint(&widgets::relative_time(workspace.updated_at, now));
    format!("{cursor} {name}  {relative}")
}

/// 一覧ブロック（見出し＋各 workspace 行＋選択中パス）を組み、端末幅 `width` に中央寄せする。
fn body_lines(width: usize, open: &Open, now: DateTime<Utc>) -> Vec<String> {
    let left_pad = " ".repeat(widgets::centered_padding(width, BLOCK_WIDTH));
    let indent = |line: &str| format!("{left_pad}{}", widgets::clip_to_width(line, BLOCK_WIDTH));

    let mut lines = vec![
        indent(&Role::Success.style().bold().paint("Workspaces")),
        String::new(),
    ];

    if open.filtered().is_empty() {
        lines.push(indent(
            &Style::new()
                .dim()
                .paint("No workspaces yet — create one from New."),
        ));
        return lines;
    }

    let mode = if open.is_unite() { "Unite" } else { "Single" };
    lines[0] = indent(
        &Role::Success
            .style()
            .bold()
            .paint(&format!("Workspaces · {mode}")),
    );
    if open.filtering() || !open.filter().is_empty() {
        lines.push(indent(&format!(
            "Filter: {}{}",
            open.filter(),
            if open.filtering() { "▌" } else { "" }
        )));
        lines.push(String::new());
    }
    for (i, workspace) in open.filtered().into_iter().enumerate() {
        let marker = if open.is_unite() && open.unite_paths.contains(&workspace.path) {
            "✓ "
        } else {
            ""
        };
        lines.push(indent(&format!(
            "{marker}{}",
            workspace_row(workspace, i == open.selected_index(), now,)
        )));
    }

    // 一覧の下に選択中 workspace の絶対パスを添える（どこを開くのか一目でわかるように）。
    if let Some(workspace) = open.selected() {
        lines.push(String::new());
        let path = format!("↳ {}", workspace.path.display());
        lines.push(indent(&Style::new().dim().paint(&path)));
    }
    if open.cleanup_confirming() {
        lines.push(String::new());
        lines.push(indent(
            &Role::Danger
                .style()
                .bold()
                .paint("Remove missing registry entries? y/n"),
        ));
    }
    lines
}

/// 生の端末サイズ `raw_height`×`raw_width` に対する Open 画面 1 フレーム分の行。
/// マスコット・タイトル・フッタの配置は共通の [`mascot_screen`] レイアウトに任せ、この関数は
/// ボディ（workspace 一覧）だけを組む。`now` は相対時刻に使うので呼び出し側が渡す。
#[must_use]
pub fn render(raw_height: usize, raw_width: usize, open: &Open, now: DateTime<Utc>) -> Vec<String> {
    mascot_screen::render(raw_height, raw_width, TITLE, FOOTER, |width| {
        body_lines(width, open, now)
    })
}

#[cfg(test)]
mod tests {
    use super::{Open, render};
    use crate::presentation::widgets::display_width;
    use chrono::{DateTime, Duration, Utc};
    use std::path::Path;
    use usagi_core::domain::workspace::Workspace;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-06-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn workspace(name: &str, minutes_ago: i64) -> Workspace {
        let mut workspace = Workspace::new(name, format!("/tmp/{name}"));
        workspace.updated_at = now() - Duration::minutes(minutes_ago);
        workspace
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

    fn rendered(open: &Open) -> String {
        render(24, 80, open, now())
            .iter()
            .map(|l| strip(l))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn new_open_starts_at_the_first_item() {
        let open = Open::new(vec![workspace("alpha", 5), workspace("beta", 10)]);
        assert_eq!(open.selected_index(), 0);
        assert_eq!(open.workspaces().len(), 2);
        assert!(!open.is_empty());
        assert_eq!(open.selected().unwrap().name, "alpha");
        // derive された Clone / Debug も計測対象なのでここで触れる。
        assert!(format!("{:?}", open.clone()).contains("Open"));
    }

    #[test]
    fn empty_open_has_no_selection() {
        let open = Open::new(Vec::new());
        assert!(open.is_empty());
        assert_eq!(open.selected(), None);
    }

    #[test]
    fn select_next_advances_and_wraps() {
        let mut open = Open::new(vec![workspace("a", 1), workspace("b", 2)]);
        open.select_next();
        assert_eq!(open.selected_index(), 1);
        open.select_next(); // wrap to 0
        assert_eq!(open.selected_index(), 0);
    }

    #[test]
    fn select_prev_wraps_to_the_last_item() {
        let mut open = Open::new(vec![workspace("a", 1), workspace("b", 2)]);
        open.select_prev();
        assert_eq!(open.selected_index(), 1);
        open.select_prev();
        assert_eq!(open.selected_index(), 0);
    }

    #[test]
    fn selection_movement_is_a_no_op_when_empty() {
        let mut open = Open::new(Vec::new());
        open.select_next();
        open.select_prev();
        assert_eq!(open.selected_index(), 0);
    }

    #[test]
    fn record_opened_updates_recency_order_and_keeps_the_selected_path() {
        let mut open = Open::new(vec![workspace("alpha", 1), workspace("beta", 10)]);
        let touched = workspace("beta", 0);

        open.record_opened(&touched);

        assert_eq!(open.workspaces()[0], touched);
        assert_eq!(open.selected_index(), 1);
        assert_eq!(open.selected().unwrap().path, Path::new("/tmp/alpha"));
    }

    #[test]
    fn render_lists_workspaces_with_their_relative_time_and_selected_path() {
        let open = Open::new(vec![workspace("alpha", 11), workspace("beta", 180)]);
        let joined = rendered(&open);
        assert!(joined.contains("Open Workspace"));
        assert!(joined.contains("Workspaces"));
        assert!(joined.contains("alpha"));
        assert!(joined.contains("beta"));
        assert!(joined.contains("11min ago"));
        // 選択中（先頭 alpha）の絶対パスが下に出る。
        assert!(joined.contains("↳ /tmp/alpha"));
        // フッタのヒント。
        assert!(joined.contains("Esc back"));
    }

    #[test]
    fn render_marks_only_the_selected_row() {
        let mut open = Open::new(vec![workspace("a", 1), workspace("b", 2)]);
        open.select_next();
        let frame = render(24, 80, &open, now());
        // カーソル ">" はちょうど 1 行に出る。
        assert_eq!(frame.iter().filter(|l| strip(l).contains('>')).count(), 1);
        // 選択が動くと下のパスも追随する。
        let joined: String = frame
            .iter()
            .map(|l| strip(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("↳ /tmp/b"));
    }

    #[test]
    fn render_shows_a_placeholder_when_there_are_no_workspaces() {
        let joined = rendered(&Open::new(Vec::new()));
        assert!(joined.contains("No workspaces yet"));
    }

    #[test]
    fn render_clips_a_long_name_and_rows_fit_the_width() {
        let open = Open::new(vec![workspace(&"x".repeat(60), 1)]);
        let frame = render(24, 80, &open, now());
        // どの行も端末幅を超えない（長い名前は省略記号で切られる）。
        assert!(frame.iter().all(|l| display_width(l) <= 80));
    }

    #[test]
    fn filter_matches_names_case_insensitively_and_keeps_selection_in_hits() {
        let mut open = Open::new(vec![workspace("alpha", 1), workspace("Beta", 2)]);
        open.begin_filter();
        open.push_filter('b');
        open.push_filter('E');

        assert_eq!(open.filter(), "bE");
        assert_eq!(open.selected().unwrap().name, "Beta");
        assert!(rendered(&open).contains("Filter: bE▌"));
        assert!(!rendered(&open).contains("↳ /tmp/alpha"));
    }

    #[test]
    fn unite_members_follow_registry_order_and_cleanup_removes_them() {
        let mut open = Open::new(vec![workspace("alpha", 1), workspace("beta", 2)]);
        open.toggle_unite();
        open.toggle_unite_member();
        open.select_next();
        open.toggle_unite_member();
        assert_eq!(
            open.unite_paths(),
            vec![
                Path::new("/tmp/alpha").to_path_buf(),
                Path::new("/tmp/beta").to_path_buf()
            ]
        );

        open.request_cleanup();
        open.remove_paths(&[Path::new("/tmp/alpha").to_path_buf()]);
        assert!(!open.cleanup_confirming());
        assert_eq!(open.workspaces().len(), 1);
        assert_eq!(
            open.unite_paths(),
            vec![Path::new("/tmp/beta").to_path_buf()]
        );
    }
}

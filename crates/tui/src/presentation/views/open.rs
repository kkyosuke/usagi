//! Open 画面（登録済み workspace を開く）。
//!
//! welcome の Open から開く画面。usagi に登録済みの workspace を一覧で並べ、選んで開く。
//! 各 row は workspace 名に続けて session 数、未完了 issue 数、最終更新の相対時刻を
//! 2 行で出し、リストの下に選択中の絶対パスを添える。
//! 状態（[`Open`]）は端末 IO を持たない純粋な値で、[`render`] が 1 フレーム分の行
//! （ANSI 付き `Vec<String>`）に変換する。マスコット・タイトル・フッタの配置は共通の
//! [`mascot_screen`] レイアウトに任せ、この view はボディ（一覧＋選択パス）だけを組む。
//!
//! 表示する workspace は永続化ストア（[`usagi_core::infrastructure::store::workspace`]）が
//! 持つが、その読み出しは実 IO なので合成ルートが行い、この層は受け取った一覧を描くだけである。
//! `now` は相対時刻に使うので呼び出し側が渡す（この層は実時計を読まない）。

use std::collections::HashSet;

use chrono::{DateTime, Utc};

use usagi_core::domain::workspace::{Workspace, WorkspaceOverview};

use crate::presentation::layouts::mascot_screen;
use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::{self, TextInput};

/// 画面上部に置くタイトル。
const TITLE: &str = "Open Workspace";
/// 最下行に固定するキー操作ヒント。
const FOOTER: &str =
    "↑↓ select / type filter / Ctrl-D unregister / Enter open / Esc back / Ctrl-C quit";
/// 一覧ブロック全体の表示幅。各行をこの幅の列に収めて桁を揃え、端末に中央寄せする。
const BLOCK_WIDTH: usize = 56;
/// workspace 名に割り当てる固定表示幅（溢れは省略記号で切る）。
const NAME_WIDTH: usize = 42;

/// Open 画面の状態。登録済み workspace の一覧と選択位置を持つ。端末 IO は持たない。
#[derive(Debug, Clone)]
pub struct Open {
    workspaces: Vec<WorkspaceOverview>,
    selected_index: usize,
    filter: TextInput,
    unite: bool,
    unite_paths: HashSet<std::path::PathBuf>,
    cleanup_confirming: bool,
    unregistering_path: Option<std::path::PathBuf>,
    unregister_confirm_selected: bool,
}

impl Open {
    /// workspace 一覧から、session などの集計値なしのメニューを組む。
    #[must_use]
    #[coverage(off)]
    pub fn new(workspaces: Vec<Workspace>) -> Self {
        Self::with_overviews(
            workspaces
                .into_iter()
                .map(|workspace| WorkspaceOverview::new(workspace, 0, 0, 0))
                .collect(),
        )
    }

    /// 集計済み overview から名前順のメニューを組む。
    #[must_use]
    #[coverage(off)]
    pub fn with_overviews(mut workspaces: Vec<WorkspaceOverview>) -> Self {
        workspaces.sort_by(|left, right| {
            left.workspace
                .name
                .to_lowercase()
                .cmp(&right.workspace.name.to_lowercase())
                .then_with(|| left.workspace.name.cmp(&right.workspace.name))
        });
        Self {
            workspaces,
            selected_index: 0,
            filter: TextInput::new(),
            unite: false,
            unite_paths: HashSet::new(),
            cleanup_confirming: false,
            unregistering_path: None,
            unregister_confirm_selected: true,
        }
    }

    /// 一覧の workspace。
    #[must_use]
    #[coverage(off)]
    pub fn workspaces(&self) -> Vec<Workspace> {
        self.workspaces
            .iter()
            .map(|overview| overview.workspace.clone())
            .collect()
    }

    /// 選択中の項目の添字。
    #[must_use]
    #[coverage(off)]
    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    /// 一覧が空か。
    #[must_use]
    #[coverage(off)]
    pub fn is_empty(&self) -> bool {
        self.workspaces.is_empty()
    }

    /// 選択中の workspace。空のときは `None`。
    #[must_use]
    #[coverage(off)]
    pub fn selected(&self) -> Option<&Workspace> {
        self.filtered()
            .get(self.selected_index)
            .map(|overview| &overview.workspace)
    }

    /// 常時表示する、大文字・小文字を区別しない workspace 名フィルタ。
    #[must_use]
    #[coverage(off)]
    pub fn filter(&self) -> &str {
        self.filter.value()
    }

    /// Whether selection builds a Unite set rather than choosing one workspace.
    #[must_use]
    #[coverage(off)]
    pub const fn is_unite(&self) -> bool {
        self.unite
    }

    /// Whether cleanup has been explicitly requested and awaits y/n confirmation.
    #[must_use]
    #[coverage(off)]
    pub const fn cleanup_confirming(&self) -> bool {
        self.cleanup_confirming
    }

    /// Path whose registry entry awaits explicit removal confirmation.
    #[must_use]
    #[coverage(off)]
    pub fn unregistering_path(&self) -> Option<&std::path::Path> {
        self.unregistering_path.as_deref()
    }

    /// Whether the destructive action is selected in the unregister modal.
    #[must_use]
    #[coverage(off)]
    pub const fn unregister_confirm_selected(&self) -> bool {
        self.unregister_confirm_selected
    }

    /// Append one character to the filter and return selection to its first hit.
    #[coverage(off)]
    pub fn push_filter(&mut self, ch: char) {
        self.filter.insert(ch);
        self.selected_index = 0;
    }

    /// Delete one filter character and return selection to its first hit.
    #[coverage(off)]
    pub fn pop_filter(&mut self) {
        self.filter.backspace();
        self.selected_index = 0;
    }

    /// Move the always-active filter cursor one character left.
    #[coverage(off)]
    pub fn filter_left(&mut self) {
        self.filter.move_left();
    }

    /// Move the always-active filter cursor one character right.
    #[coverage(off)]
    pub fn filter_right(&mut self) {
        self.filter.move_right();
    }

    /// Switch between Single and Unite selection. A new Unite set starts empty.
    #[coverage(off)]
    pub fn toggle_unite(&mut self) {
        self.unite = !self.unite;
        self.unite_paths.clear();
    }

    /// Add or remove the selected workspace from the Unite set.
    #[coverage(off)]
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
    #[coverage(off)]
    pub fn unite_paths(&self) -> Vec<std::path::PathBuf> {
        self.workspaces
            .iter()
            .filter(|overview| self.unite_paths.contains(&overview.workspace.path))
            .map(|overview| overview.workspace.path.clone())
            .collect()
    }

    /// Ask for an explicit cleanup confirmation.
    #[coverage(off)]
    pub fn request_cleanup(&mut self) {
        self.cleanup_confirming = true;
    }

    /// Dismiss a cleanup confirmation without mutating the registry.
    #[coverage(off)]
    pub fn cancel_cleanup(&mut self) {
        self.cleanup_confirming = false;
    }

    /// Ask for explicit confirmation before removing the selected registry entry.
    ///
    /// This records only the entry path. The caller owns the actual registry
    /// mutation, so this view can never delete the workspace directory or data.
    #[coverage(off)]
    pub fn request_unregister(&mut self) {
        self.unregistering_path = self.selected().map(|workspace| workspace.path.clone());
        self.unregister_confirm_selected = true;
    }

    /// Dismiss a selected-entry unregister confirmation without mutation.
    #[coverage(off)]
    pub fn cancel_unregister(&mut self) {
        self.unregistering_path = None;
        self.unregister_confirm_selected = true;
    }

    /// Move the unregister modal focus between its confirm and cancel buttons.
    #[coverage(off)]
    pub fn toggle_unregister_choice(&mut self) {
        self.unregister_confirm_selected = !self.unregister_confirm_selected;
    }

    /// Consume the path selected for confirmed registry removal.
    #[coverage(off)]
    pub fn confirm_unregister(&mut self) -> Option<std::path::PathBuf> {
        if !self.unregister_confirm_selected {
            self.cancel_unregister();
            return None;
        }
        self.unregistering_path.take()
    }

    /// Finish a confirmed cleanup, removing the returned registry paths locally.
    #[coverage(off)]
    pub fn remove_paths(&mut self, paths: &[std::path::PathBuf]) {
        self.workspaces
            .retain(|overview| !paths.iter().any(|path| path == &overview.workspace.path));
        self.unite_paths.retain(|path| !paths.contains(path));
        self.cleanup_confirming = false;
        self.unregistering_path = None;
        self.unregister_confirm_selected = true;
        self.selected_index = 0;
    }

    #[coverage(off)]
    fn filtered(&self) -> Vec<&WorkspaceOverview> {
        let filter = self.filter.value().to_lowercase();
        self.workspaces
            .iter()
            .filter(|overview| overview.workspace.name.to_lowercase().contains(&filter))
            .collect()
    }

    /// `workspace` と同じ path の項目に touch 後の値を反映する。
    ///
    /// Open list は名前順なので、最終利用時刻の更新で行を並べ替えない。
    #[coverage(off)]
    pub(crate) fn record_opened(&mut self, workspace: &Workspace) {
        let selected_path = self.selected().map(|selected| selected.path.clone());
        let Some(current) = self
            .workspaces
            .iter_mut()
            .find(|current| current.workspace.path == workspace.path)
        else {
            return;
        };
        current.workspace = workspace.clone();
        self.selected_index = selected_path
            .and_then(|selected_path| {
                self.filtered()
                    .iter()
                    .position(|current| current.workspace.path == selected_path)
            })
            .unwrap_or_default();
    }

    /// 選択を 1 つ下へ（末尾から先頭へ回り込む）。空一覧では何もしない。
    #[coverage(off)]
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
    #[coverage(off)]
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
#[coverage(off)]
fn fit(text: &str, width: usize) -> String {
    let clipped = widgets::clip_to_width(text, width);
    let visible = widgets::display_width(&clipped);
    format!("{clipped}{}", " ".repeat(width.saturating_sub(visible)))
}

/// 一覧の名前行。選択行はカーソルと名前を強調する。
#[coverage(off)]
fn workspace_name_row(workspace: &Workspace, is_selected: bool) -> String {
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
    format!("{cursor} {name}")
}

/// 一覧の名前行の下に置く、workspace の状態をひと目で読める補助行。
#[coverage(off)]
fn workspace_stats_row(overview: &WorkspaceOverview, now: DateTime<Utc>) -> String {
    let sessions = overview.session_count;
    let session_label = if sessions == 1 { "session" } else { "sessions" };
    let updated = widgets::relative_time(overview.workspace.updated_at, now);
    Style::new().dim().paint(&format!(
        "    ⎇ {sessions} {session_label}  ·  ● {} open  ·  ◷ updated {updated}",
        overview.open_issue_count,
    ))
}

/// 共通 [`TextInput`] の編集位置を明示した、常時フォーカスされる Filter 行。
#[coverage(off)]
fn filter_line(open: &Open) -> String {
    let input = &open.filter;
    // 共通 widget が input 全体を accent で描き、編集位置だけを block cursor にする。
    let accent = Role::Accent.style().bold();
    let value = format!(
        "{}{}",
        widgets::block_caret(input.value(), input.cursor(), &accent),
        if input.is_empty() {
            Role::Accent.style().dim().paint("type to filter")
        } else {
            String::new()
        },
    );
    format!("{} {value}", accent.paint("Filter:"))
}

/// 一覧ブロック（見出し＋各 workspace 行＋選択中パス）を組み、端末幅 `width` に中央寄せする。
#[coverage(off)]
fn body_lines(width: usize, open: &Open, now: DateTime<Utc>) -> Vec<String> {
    let left_pad = " ".repeat(widgets::centered_padding(width, BLOCK_WIDTH));
    let indent = |line: &str| format!("{left_pad}{}", widgets::clip_to_width(line, BLOCK_WIDTH));

    let mut lines = vec![
        indent(&Role::Success.style().bold().paint("Workspaces")),
        String::new(),
    ];

    lines.push(indent(&filter_line(open)));
    lines.push(String::new());

    if open.is_empty() {
        lines.push(indent(
            &Style::new()
                .dim()
                .paint("No workspaces yet — create one from New."),
        ));
        return lines;
    }
    if open.filtered().is_empty() {
        lines.push(indent(
            &Style::new().dim().paint("No workspaces match the filter."),
        ));
        return lines;
    }
    for (i, overview) in open.filtered().into_iter().enumerate() {
        let marker = if open.is_unite() && open.unite_paths.contains(&overview.workspace.path) {
            Role::Success.style().bold().paint("✓ ")
        } else {
            String::new()
        };
        lines.push(indent(&format!(
            "{marker}{}",
            workspace_name_row(&overview.workspace, i == open.selected_index())
        )));
        lines.push(indent(&workspace_stats_row(overview, now)));
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
#[coverage(off)]
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
    use usagi_core::domain::workspace::{Workspace, WorkspaceOverview};

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
            if !matches!(ch, '\u{e0001}' | '\u{e0002}') {
                out.push(ch);
            }
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
    fn record_opened_keeps_alphabetical_order_and_the_selected_path() {
        let mut open = Open::new(vec![workspace("alpha", 1), workspace("beta", 10)]);
        let touched = workspace("beta", 0);

        open.record_opened(&touched);

        assert_eq!(open.workspaces()[1], touched);
        assert_eq!(open.selected_index(), 0);
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
        assert!(joined.contains("⎇ 0 sessions"));
        assert!(joined.contains("updated 11min ago"));
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
        assert!(joined.contains("Filter:"));
        assert!(joined.contains("type to filter"));
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
        open.push_filter('b');
        open.push_filter('E');

        assert_eq!(open.filter(), "bE");
        assert_eq!(open.selected().unwrap().name, "Beta");
        assert!(rendered(&open).contains("Filter: bE"));
        assert!(!rendered(&open).contains("↳ /tmp/alpha"));
    }

    #[test]
    fn filter_stays_visible_when_it_has_no_matches() {
        let mut open = Open::new(vec![workspace("alpha", 1)]);
        open.push_filter('z');

        let joined = rendered(&open);
        assert!(joined.contains("Filter: z"));
        assert!(joined.contains("No workspaces match the filter."));
    }

    #[test]
    fn filter_renders_the_shared_input_cursor_at_its_edit_position() {
        let mut open = Open::new(vec![workspace("alpha", 1)]);
        open.push_filter('a');
        open.push_filter('b');
        open.filter_left();
        open.push_filter('x');

        assert_eq!(open.filter(), "axb");
        let frame = render(24, 80, &open, now()).join("\n");
        assert!(rendered(&open).contains("Filter: axb"));
        assert!(frame.contains("\u{1b}[1;36max\u{1b}[0m"));
        // The shared block cursor reverses the character at the edit position
        // without shifting the remaining input.
        assert!(frame.contains("\u{1b}[1;7;36mb\u{1b}[0m"));
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

    #[test]
    fn unregister_confirmation_keeps_files_out_of_the_view_state_and_removes_only_confirmed_path() {
        let mut open = Open::new(vec![workspace("alpha", 1), workspace("beta", 2)]);
        open.select_next();

        open.request_unregister();
        assert_eq!(open.unregistering_path(), Some(Path::new("/tmp/beta")));
        assert!(open.unregister_confirm_selected());

        open.toggle_unregister_choice();
        assert!(!open.unregister_confirm_selected());
        assert_eq!(open.confirm_unregister(), None);
        assert_eq!(open.unregistering_path(), None);

        assert_eq!(open.workspaces().len(), 2);

        open.request_unregister();
        let path = open.confirm_unregister().unwrap();
        assert_eq!(path, Path::new("/tmp/beta"));
        open.remove_paths(&[path]);
        assert_eq!(open.workspaces().len(), 1);
        assert_eq!(open.selected().unwrap().path, Path::new("/tmp/alpha"));
    }

    #[test]
    fn footer_advertises_unregister_without_stale_unite_or_cleanup_hints() {
        let footer = rendered(&Open::new(vec![workspace("alpha", 1)]));

        assert!(footer.contains("Ctrl-D unregister"));
        assert!(!footer.contains("Tab Unite"));
        assert!(!footer.contains("C cleanup"));
    }

    #[test]
    fn overview_rows_sort_case_insensitively_and_show_live_figures() {
        let mut zeta = workspace("zeta", 5);
        zeta.updated_at = now() - Duration::hours(2);
        let alpha = workspace("Alpha", 5);
        let open = Open::with_overviews(vec![
            WorkspaceOverview::new(zeta, 3, 2, 1),
            WorkspaceOverview::new(alpha, 1, 0, 0),
        ]);

        assert_eq!(open.workspaces()[0].name, "Alpha");
        let joined = rendered(&open);
        assert!(joined.contains("⎇ 3 sessions  ·  ● 2 open  ·  ◷ updated 2h ago"));
        assert!(joined.contains("Workspaces"));
        assert!(!joined.contains("A–Z"));
    }
}

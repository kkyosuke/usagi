//! New 画面（新規 workspace 作成）。
//!
//! welcome から開く「新しいプロジェクトを始める」画面。上部で 2 つの作り方を切り替える:
//!
//! - **Clone** — Git リポジトリを新しいディレクトリへクローンする。URL を打つと、まだ手で
//!   編集していない間はディレクトリ名を URL から自動導出する。
//! - **Existing** — ディスク上の既存ディレクトリを workspace として登録する。名前はパスの
//!   末尾から自動導出する（手で編集するまで）。
//!
//! マスコット＋タイトル＋フッタの配置は共通の [`mascot_screen`] レイアウトに任せ、この view は
//! ボディ（モード切替タブ・入力フィールド・通知）だけを組む。状態（[`New`]）は端末 IO を持たない
//! 純粋な値で、[`render`] が 1 フレーム分の行（ANSI 付き `Vec<String>`）に変換する。
//!
//! キー入力の解釈と、フォームの検証・確定（`NewProject` への変換）は入力・usecase 層が整うときに
//! 載せる。ここでは編集に必要な純粋操作 — フォーカス移動・モード切替・キャレット編集 — だけを
//! 公開する。

use crate::presentation::layouts::mascot_screen;
use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::{self, TextInput};

/// 画面上部に置くタイトル。
const TITLE: &str = "New Project";
/// タイトル下に出す説明。
const SUBTITLE: &str = "Clone a repository or register an existing directory";
/// 中央寄せするフォームブロックの表示幅。全フィールドをこの幅の列に収めて桁を揃える。
const BLOCK_WIDTH: usize = 52;
/// フィールドブロックの予約行数。背の高い Clone（4 フィールド = ラベル＋入力の 2 行 × 4 ＋
/// 区切り 3 行）に合わせ、両モードともこの高さに詰めてモード切替でブロックが伸縮しないようにする。
const RESERVED_FIELDS_HEIGHT: usize = 11;

/// フォームが作るプロジェクトの種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Git リポジトリを新しいディレクトリへクローンする。
    #[default]
    Clone,
    /// ディスク上の既存ディレクトリを登録する。
    Existing,
}

impl Mode {
    /// もう一方のモード（2 つしかないので切替はトグル）。
    #[must_use]
    pub fn other(self) -> Mode {
        match self {
            Mode::Clone => Mode::Existing,
            Mode::Existing => Mode::Clone,
        }
    }

    /// このモードのフォーカス可能フィールドを Tab 順に返す。モード選択タブが常に先頭なので
    /// Tab で選択タブに戻ってモードを切り替えられる。
    fn fields(self) -> &'static [Field] {
        match self {
            Mode::Clone => &[
                Field::Mode,
                Field::Url,
                Field::Location,
                Field::Directory,
                Field::Branch,
            ],
            Mode::Existing => &[Field::Mode, Field::Path, Field::Name],
        }
    }
}

/// フォームのフォーカス可能要素。どれが有効かは [`Mode`] による。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// モード選択タブそのもの。
    Mode,
    /// Clone: リポジトリ URL。
    Url,
    /// Clone: クローン先の親ディレクトリ。
    Location,
    /// Clone: 作成するディレクトリ名。
    Directory,
    /// Clone: チェックアウトするブランチ（任意）。
    Branch,
    /// Existing: 登録する既存ディレクトリのパス。
    Path,
    /// Existing: workspace 名。
    Name,
}

/// New 画面の編集状態。端末 IO を持たず、[`render`] に渡して描画する。
#[derive(Debug, Clone, Default)]
pub struct New {
    mode: Mode,
    /// `mode.fields()` 内のフォーカス位置。
    focus_index: usize,

    // Clone モードの入力。
    url: TextInput,
    location: TextInput,
    directory: TextInput,
    branch: TextInput,
    /// ディレクトリを手で編集したら自動導出を止める。
    directory_dirty: bool,

    // Existing モードの入力。
    path: TextInput,
    name: TextInput,
    /// 名前を手で編集したら自動導出を止める。
    name_dirty: bool,

    notice: Option<String>,
    /// Directory-path fields に対して Tab が見つけた候補。複数候補だけを表示し、入力値は
    /// そのまま保つので、続きを打って絞り込める。
    directory_matches: Vec<String>,
}

impl New {
    /// 現在のモード。
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// リポジトリ URL（Clone）。
    #[must_use]
    pub fn url(&self) -> &str {
        self.url.value()
    }

    /// クローン先の親ディレクトリ（Clone）。
    #[must_use]
    pub fn location(&self) -> &str {
        self.location.value()
    }

    /// 作成するディレクトリ名（Clone）。
    #[must_use]
    pub fn directory(&self) -> &str {
        self.directory.value()
    }

    /// チェックアウトするブランチ（Clone、任意）。
    #[must_use]
    pub fn branch(&self) -> &str {
        self.branch.value()
    }

    /// 登録する既存ディレクトリのパス（Existing）。
    #[must_use]
    pub fn path(&self) -> &str {
        self.path.value()
    }

    /// workspace 名（Existing）。
    #[must_use]
    pub fn name(&self) -> &str {
        self.name.value()
    }

    /// 現在フォーカスしているフィールド。
    #[must_use]
    pub fn focus(&self) -> Field {
        self.mode.fields()[self.focus_index]
    }

    /// フォーカス中フィールドのキャレット位置（バイトオフセット）。モード選択は入力欄でないので 0。
    #[must_use]
    pub fn focus_cursor(&self) -> usize {
        self.focused_input().map_or(0, TextInput::cursor)
    }

    /// 一時的な通知（検証エラーなど）。
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// 通知を差し替える。
    pub fn set_notice(&mut self, notice: Option<String>) {
        self.notice = notice;
    }

    /// フォーカス中フィールドの [`TextInput`]、モード選択なら `None`。編集・キャレット移動は
    /// すべてここを通して 1 実装を共有する。
    fn field_mut(&mut self, field: Field) -> Option<&mut TextInput> {
        match field {
            Field::Mode => None,
            Field::Url => Some(&mut self.url),
            Field::Location => Some(&mut self.location),
            Field::Directory => Some(&mut self.directory),
            Field::Branch => Some(&mut self.branch),
            Field::Path => Some(&mut self.path),
            Field::Name => Some(&mut self.name),
        }
    }

    /// フォーカス中フィールドの [`TextInput`]（参照）。モード選択なら `None`。
    fn focused_input(&self) -> Option<&TextInput> {
        match self.focus() {
            Field::Mode => None,
            Field::Url => Some(&self.url),
            Field::Location => Some(&self.location),
            Field::Directory => Some(&self.directory),
            Field::Branch => Some(&self.branch),
            Field::Path => Some(&self.path),
            Field::Name => Some(&self.name),
        }
    }

    /// もう一方のモードへ切り替える。フォーカスはモード選択に戻すので、続けて切替できる。
    pub fn toggle_mode(&mut self) {
        self.mode = self.mode.other();
        self.focus_index = 0;
    }

    /// フォーカスを次のフィールドへ（末尾で先頭へ回り込む）。
    pub fn focus_next(&mut self) {
        let len = self.mode.fields().len();
        self.focus_index = (self.focus_index + 1) % len;
        self.directory_matches.clear();
    }

    /// フォーカスを前のフィールドへ（先頭で末尾へ回り込む）。
    pub fn focus_prev(&mut self) {
        let len = self.mode.fields().len();
        self.focus_index = (self.focus_index + len - 1) % len;
        self.directory_matches.clear();
    }

    /// フォーカス中フィールドのキャレット位置に 1 文字挿入する。モード選択では何もしない。
    pub fn insert_char(&mut self, c: char) {
        let field = self.focus();
        if let Some(input) = self.field_mut(field) {
            input.insert(c);
        }
        self.after_edit(field);
    }

    /// Clone の Location または Existing の Directory を、ファイルシステム上の子
    /// ディレクトリで補完する。候補が 1 件ならその値を入力欄へ反映し、複数なら候補を画面に
    /// 表示して続きを入力できるようにする。
    pub fn complete_directory(&mut self) {
        let field = self.focus();
        if !matches!(field, Field::Location | Field::Path) {
            self.focus_next();
            return;
        }

        let value = self.focused_input().map_or("", TextInput::value);
        let (parent, prefix) = directory_completion_base(value);
        let Ok(entries) = std::fs::read_dir(&parent) else {
            self.directory_matches.clear();
            return;
        };

        let mut matches = entries
            .flatten()
            .filter_map(|entry| entry.file_type().ok()?.is_dir().then(|| entry.file_name()))
            .filter_map(|name| name.into_string().ok())
            .filter(|name| name.starts_with(&prefix))
            .map(|name| format_directory_candidate(value, &parent, &name))
            .collect::<Vec<_>>();
        matches.sort();

        if matches.len() == 1 {
            let completed = format!("{}/", matches[0]);
            if let Some(input) = self.field_mut(field) {
                input.set_value(completed);
                self.after_edit(field);
            }
        }
        self.directory_matches = matches;
    }

    /// フォーカス中フィールドのキャレット手前の 1 文字を削除する。
    pub fn backspace(&mut self) {
        let field = self.focus();
        if let Some(input) = self.field_mut(field) {
            input.backspace();
        }
        self.after_edit(field);
    }

    /// フォーカス中フィールドのキャレット位置の 1 文字を削除する（Del）。
    pub fn delete_forward(&mut self) {
        let field = self.focus();
        if let Some(input) = self.field_mut(field) {
            input.delete_forward();
        }
        self.after_edit(field);
    }

    /// キャレットを 1 文字左へ。
    pub fn cursor_left(&mut self) {
        let field = self.focus();
        if let Some(input) = self.field_mut(field) {
            input.move_left();
        }
    }

    /// キャレットを 1 文字右へ。
    pub fn cursor_right(&mut self) {
        let field = self.focus();
        if let Some(input) = self.field_mut(field) {
            input.move_right();
        }
    }

    /// キャレットを行頭へ。
    pub fn cursor_home(&mut self) {
        let field = self.focus();
        if let Some(input) = self.field_mut(field) {
            input.move_home();
        }
    }

    /// キャレットを行末へ。
    pub fn cursor_end(&mut self) {
        let field = self.focus();
        if let Some(input) = self.field_mut(field) {
            input.move_end();
        }
    }

    /// テキストが変わったフィールドの自動導出をやり直す。URL はディレクトリを、パスは名前を
    /// 導出する。ディレクトリ・名前は手編集済みか（非空 ⇒ dirty）を追い、空にすると自動導出へ
    /// 戻る（エディタが候補を復活させる挙動に合わせる）。
    fn after_edit(&mut self, field: Field) {
        self.directory_matches.clear();
        match field {
            Field::Url => self.sync_directory(),
            Field::Directory => self.directory_dirty = !self.directory.is_empty(),
            Field::Path => self.sync_name(),
            Field::Name => self.name_dirty = !self.name.is_empty(),
            _ => {}
        }
    }

    /// 手編集していなければ URL からディレクトリ名を導出する。
    fn sync_directory(&mut self) {
        if self.directory_dirty {
            return;
        }
        self.directory
            .set_value(suggest_directory(self.url.value()));
    }

    /// 手編集していなければパスから workspace 名を導出する。
    fn sync_name(&mut self) {
        if self.name_dirty {
            return;
        }
        self.name.set_value(suggest_name(self.path.value()));
    }
}

/// Split a partially entered path into the directory to enumerate and the child-name prefix.
fn directory_completion_base(value: &str) -> (std::path::PathBuf, String) {
    let path = std::path::Path::new(value);
    if value.ends_with(std::path::MAIN_SEPARATOR) {
        return (path.to_path_buf(), String::new());
    }
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let prefix = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    (parent.to_path_buf(), prefix)
}

/// Preserve the user's relative/absolute spelling while appending a matching child directory.
fn format_directory_candidate(value: &str, parent: &std::path::Path, name: &str) -> String {
    if value.ends_with(std::path::MAIN_SEPARATOR) {
        return format!("{value}{name}");
    }
    if parent == std::path::Path::new(".") {
        return name.to_owned();
    }
    parent.join(name).to_string_lossy().into_owned()
}

/// リポジトリ URL から作成ディレクトリ名を提案する: 末尾のパス要素から `.git` を除いたもの。
/// `https://github.com/owner/repo.git` も `git@github.com:owner/repo.git` も `repo` になる。
fn suggest_directory(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    let last = trimmed.rsplit(['/', ':']).next().unwrap_or("");
    last.strip_suffix(".git").unwrap_or(last).to_string()
}

/// ディレクトリパスから workspace 名を提案する: 末尾の要素。末尾スラッシュ（`/a/b/` → `b`）や
/// 空入力（`""` → `""`）に強い。
fn suggest_name(path: &str) -> String {
    std::path::Path::new(path.trim())
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// フォーカス中の入力欄のテキスト。共通 block cursor を使い、編集位置の 1 文字を
/// reverse-video で示す。空・行末は反転空白 1 つになる。
fn caret_text(value: &str, cursor: usize) -> String {
    let accent = Role::Accent.style().bold();
    widgets::block_caret(value, cursor, &accent)
}

/// 1 入力行: フォーカス中は `>` カーソル、値（空なら dim のプレースホルダ）、フォーカス中は
/// キャレットを描く。
fn input_line(
    block_pad: &str,
    value: &str,
    cursor: usize,
    placeholder: &str,
    focused: bool,
) -> String {
    let marker = if focused {
        Role::Danger.style().bold().paint(">")
    } else {
        " ".to_string()
    };
    let body = if value.is_empty() {
        if focused {
            caret_text("", 0)
        } else {
            Style::new().dim().italic().paint(placeholder)
        }
    } else if focused {
        caret_text(value, cursor)
    } else {
        Role::Accent.style().paint(value)
    };
    format!("{block_pad}{marker} {body}")
}

/// モード選択: 2 つのタブ（Clone / Existing）。有効側を強調し、フォーカス中は `>` を付ける。
fn mode_lines(block_pad: &str, mode: Mode, focused: bool) -> Vec<String> {
    let tab = |label: &str, active: bool| {
        if active {
            format!("[{}]", Role::Accent.style().bold().paint(label))
        } else {
            format!(" {} ", Style::new().dim().paint(label))
        }
    };
    let marker = if focused {
        Role::Danger.style().bold().paint(">")
    } else {
        " ".to_string()
    };
    let tabs = format!(
        "{}  {}",
        tab("Clone", mode == Mode::Clone),
        tab("Existing", mode == Mode::Existing),
    );
    vec![
        format!(
            "{block_pad}{}",
            Style::new().dim().paint("Type  (←→ to switch)")
        ),
        format!("{block_pad}{marker} {tabs}"),
    ]
}

/// ラベル付きフィールド: dim のラベル行＋入力行。
fn field_lines(
    block_pad: &str,
    label: &str,
    value: &str,
    cursor: usize,
    placeholder: &str,
    focused: bool,
) -> Vec<String> {
    vec![
        format!("{block_pad}{}", Style::new().dim().paint(label)),
        input_line(block_pad, value, cursor, placeholder, focused),
    ]
}

/// Clone モードのフィールド（URL / Location / Directory / Branch）。
fn clone_fields(block_pad: &str, state: &New) -> Vec<String> {
    let caret = state.focus_cursor();
    let mut lines = field_lines(
        block_pad,
        "Repository URL",
        state.url(),
        caret,
        "https://github.com/owner/repo.git",
        state.focus() == Field::Url,
    );
    lines.push(String::new());
    lines.extend(field_lines(
        block_pad,
        "Location",
        state.location(),
        caret,
        "where to create the project",
        state.focus() == Field::Location,
    ));
    lines.push(String::new());
    lines.extend(field_lines(
        block_pad,
        "Directory",
        state.directory(),
        caret,
        "derived from the URL",
        state.focus() == Field::Directory,
    ));
    lines.push(String::new());
    lines.extend(field_lines(
        block_pad,
        "Branch (optional)",
        state.branch(),
        caret,
        "repository default",
        state.focus() == Field::Branch,
    ));
    lines
}

/// Existing モードのフィールド（Directory path / Name）。
fn existing_fields(block_pad: &str, state: &New) -> Vec<String> {
    let caret = state.focus_cursor();
    let mut lines = field_lines(
        block_pad,
        "Directory",
        state.path(),
        caret,
        "/path/to/an/existing/project",
        state.focus() == Field::Path,
    );
    lines.push(String::new());
    lines.extend(field_lines(
        block_pad,
        "Name",
        state.name(),
        caret,
        "derived from the directory",
        state.focus() == Field::Name,
    ));
    lines
}

/// 現在のモードのフィールドブロックを [`RESERVED_FIELDS_HEIGHT`] に詰めて返す。Clone は 4
/// フィールド、Existing は 2 なので、詰めないとモード切替（←→）で中央寄せがずれる。両モードを
/// 同じ高さに保ち、ヘッダ・選択タブ・フッタを切替後も固定する。
fn fields_lines(block_pad: &str, state: &New) -> Vec<String> {
    let mut lines = match state.mode() {
        Mode::Clone => clone_fields(block_pad, state),
        Mode::Existing => existing_fields(block_pad, state),
    };
    lines.resize(lines.len().max(RESERVED_FIELDS_HEIGHT), String::new());
    lines
}

/// フォーム下の通知（検証エラー）。常に 2 行（空の区切り＋通知スロット）返し、出現・消滅で
/// フォームがずれないようにする。
fn notice_lines(block_pad: &str, notice: Option<&str>) -> Vec<String> {
    let slot = match notice {
        Some(notice) => format!("{block_pad}{}", Role::Danger.style().bold().paint(notice)),
        None => String::new(),
    };
    vec![String::new(), slot]
}

/// Tab による複数のディレクトリ候補。候補がない場合も同じ高さを確保する。
fn directory_match_lines(block_pad: &str, matches: &[String]) -> Vec<String> {
    let value = if matches.len() > 1 {
        let label = format!(
            "Matches: {}",
            matches
                .iter()
                .take(5)
                .map(|candidate| format!("{candidate}/"))
                .collect::<Vec<_>>()
                .join("  ")
        );
        format!("{block_pad}{}", Style::new().dim().paint(&label))
    } else {
        String::new()
    };
    vec![value]
}

/// フォーカス中フィールドに応じたフッタのヒント。モード選択では ←→ が種類切替、テキスト欄では
/// キャレット移動を意味するので、現在のフィールドがすることだけを書く。
fn footer_hint(state: &New) -> &'static str {
    if state.focus() == Field::Mode {
        "←→: switch type / ↑↓/Tab: move field / Enter: create / Esc: back"
    } else if matches!(state.focus(), Field::Location | Field::Path) {
        "Tab: complete directory / ←→: move caret / ↑↓: move field / Enter: create / Esc: back"
    } else {
        "←→: move caret / ↑↓/Tab: move field / Enter: create / Esc: back"
    }
}

/// 生の端末サイズに対する New 画面 1 フレーム分の行。マスコット・タイトル・フッタの配置は共通の
/// [`mascot_screen`] レイアウトに任せ、この関数はボディ（説明・モード切替・フィールド・通知）を組む。
#[must_use]
pub fn render(raw_height: usize, raw_width: usize, state: &New) -> Vec<String> {
    mascot_screen::render(raw_height, raw_width, TITLE, footer_hint(state), |width| {
        let block_pad = " ".repeat(widgets::centered_padding(width, BLOCK_WIDTH));
        let mut body = vec![
            mascot_screen::centered_line(width, SUBTITLE, Style::new().dim()),
            String::new(),
        ];
        body.extend(mode_lines(
            &block_pad,
            state.mode(),
            state.focus() == Field::Mode,
        ));
        body.push(String::new());
        body.extend(fields_lines(&block_pad, state));
        body.extend(directory_match_lines(&block_pad, &state.directory_matches));
        body.extend(notice_lines(&block_pad, state.notice()));
        body
    })
}

#[cfg(test)]
mod tests {
    use super::{
        Field, Mode, New, directory_completion_base, format_directory_candidate, render,
        suggest_directory, suggest_name,
    };

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

    fn joined(state: &New) -> String {
        render(0, 0, state)
            .iter()
            .map(|l| strip(l))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn type_str(state: &mut New, text: &str) {
        for c in text.chars() {
            state.insert_char(c);
        }
    }

    #[test]
    fn new_form_defaults_to_clone_mode_focused_on_the_selector() {
        let state = New::default();
        assert_eq!(state.mode(), Mode::Clone);
        assert_eq!(state.focus(), Field::Mode);
        assert_eq!(state.focus_cursor(), 0);
        assert_eq!(state.notice(), None);
        // derive された Clone / Debug も計測対象なので触れる。
        assert!(format!("{:?}", state.clone()).contains("New"));
    }

    #[test]
    fn mode_other_toggles_and_default_is_clone() {
        assert_eq!(Mode::Clone.other(), Mode::Existing);
        assert_eq!(Mode::Existing.other(), Mode::Clone);
        assert_eq!(Mode::default(), Mode::Clone);
        // Field / Mode の derive も。
        assert_eq!(Field::Url, Field::Url);
        assert!(format!("{:?}", Field::Mode).contains("Mode"));
        assert!(format!("{:?}", Mode::Clone).contains("Clone"));
    }

    #[test]
    fn focus_next_and_prev_cycle_through_the_mode_fields() {
        let mut state = New::default();
        assert_eq!(state.focus(), Field::Mode);
        state.focus_next();
        assert_eq!(state.focus(), Field::Url);
        state.focus_prev();
        assert_eq!(state.focus(), Field::Mode);
        // 先頭で prev すると末尾（Branch）へ回り込む。
        state.focus_prev();
        assert_eq!(state.focus(), Field::Branch);
        state.focus_next();
        assert_eq!(state.focus(), Field::Mode);
    }

    #[test]
    fn toggle_mode_switches_fields_and_refocuses_the_selector() {
        let mut state = New::default();
        state.focus_next(); // Url
        state.toggle_mode();
        assert_eq!(state.mode(), Mode::Existing);
        // 切替でフォーカスはモード選択に戻る。
        assert_eq!(state.focus(), Field::Mode);
        state.focus_next();
        assert_eq!(state.focus(), Field::Path);
        state.focus_next();
        assert_eq!(state.focus(), Field::Name);
    }

    #[test]
    fn typing_the_url_derives_the_directory_until_hand_edited() {
        let mut state = New::default();
        state.focus_next(); // Url
        type_str(&mut state, "https://github.com/owner/repo.git");
        assert_eq!(state.url(), "https://github.com/owner/repo.git");
        // ディレクトリは URL から自動導出。
        assert_eq!(state.directory(), "repo");
        // ディレクトリを手編集すると自動導出が止まる。
        state.focus_next(); // Location
        state.focus_next(); // Directory
        assert_eq!(state.focus(), Field::Directory);
        type_str(&mut state, "custom");
        assert_eq!(state.directory(), "repocustom");
        // URL を変えても手編集済みなら追随しない。
        // （直接 Url へ戻して 1 文字消す）
        state.focus_prev(); // Location
        state.focus_prev(); // Url
        state.cursor_end();
        state.backspace();
        assert_eq!(state.directory(), "repocustom");
    }

    #[test]
    fn clearing_the_directory_restores_auto_derivation() {
        let mut state = New::default();
        state.focus_next(); // Url
        type_str(&mut state, "git@github.com:owner/proj.git");
        assert_eq!(state.directory(), "proj");
        // ディレクトリを手編集 → dirty。
        state.focus_next(); // Location
        state.focus_next(); // Directory
        state.insert_char('x');
        // 空にすると dirty が解除され、次の URL 変更で再び導出される。
        state.backspace(); // remove 'x'
        state.cursor_end();
        for _ in 0.."projx".len() {
            state.backspace();
        }
        assert!(state.directory().is_empty());
        state.focus_prev(); // Location
        state.focus_prev(); // Url
        state.insert_char('/');
        // URL 末尾が変わり、dirty 解除済みなので再導出（"proj.git/" → "proj"）。
        assert_eq!(state.directory(), "proj");
    }

    #[test]
    fn existing_mode_derives_the_name_from_the_path() {
        let mut state = New::default();
        state.toggle_mode();
        state.focus_next(); // Path
        type_str(&mut state, "/home/user/my-app");
        assert_eq!(state.path(), "/home/user/my-app");
        assert_eq!(state.name(), "my-app");
        // 名前を手編集すると追随を止める。
        state.focus_next(); // Name
        state.insert_char('!');
        assert_eq!(state.name(), "my-app!");
        state.focus_prev(); // Path
        state.cursor_end();
        state.backspace();
        assert_eq!(state.name(), "my-app!");
    }

    #[test]
    fn caret_editing_moves_and_deletes_within_the_focused_field() {
        let mut state = New::default();
        state.focus_next(); // Url
        type_str(&mut state, "abc");
        assert_eq!(state.focus_cursor(), 3);
        state.cursor_home();
        assert_eq!(state.focus_cursor(), 0);
        state.cursor_right();
        assert_eq!(state.focus_cursor(), 1);
        state.delete_forward(); // remove 'b'
        assert_eq!(state.url(), "ac");
        state.cursor_end();
        state.cursor_left();
        assert_eq!(state.focus_cursor(), 1);
        // モード選択にフォーカスがあるとき編集操作は no-op。
        state.focus_prev(); // Mode
        assert_eq!(state.focus(), Field::Mode);
        state.insert_char('z');
        state.backspace();
        state.delete_forward();
        state.cursor_left();
        state.cursor_right();
        state.cursor_home();
        state.cursor_end();
        assert_eq!(state.focus_cursor(), 0);
    }

    #[test]
    fn set_notice_replaces_the_notice() {
        let mut state = New::default();
        state.set_notice(Some("bad url".to_string()));
        assert_eq!(state.notice(), Some("bad url"));
        state.set_notice(None);
        assert_eq!(state.notice(), None);
    }

    #[test]
    fn suggest_helpers_take_the_last_segment() {
        assert_eq!(
            suggest_directory("https://github.com/owner/repo.git"),
            "repo"
        );
        assert_eq!(suggest_directory("git@github.com:owner/repo.git"), "repo");
        assert_eq!(suggest_directory("https://example.com/x/y/"), "y");
        assert_eq!(suggest_directory("   "), "");
        assert_eq!(suggest_name("/a/b/c"), "c");
        assert_eq!(suggest_name("/a/b/"), "b");
        assert_eq!(suggest_name(""), "");
    }

    #[test]
    fn directory_completion_preserves_relative_paths_and_splits_the_typed_prefix() {
        assert_eq!(
            directory_completion_base("/tmp/pro"),
            (std::path::PathBuf::from("/tmp"), "pro".to_owned())
        );
        assert_eq!(
            directory_completion_base("/tmp/"),
            (std::path::PathBuf::from("/tmp/"), String::new())
        );
        assert_eq!(
            format_directory_candidate("pro", std::path::Path::new("."), "project"),
            "project"
        );
    }

    #[test]
    fn tab_completion_completes_one_directory_and_displays_multiple_matches() {
        let temporary = tempfile::tempdir().unwrap();
        std::fs::create_dir(temporary.path().join("alpha")).unwrap();

        let mut state = New::default();
        state.toggle_mode();
        state.focus_next(); // Path
        type_str(&mut state, &temporary.path().join("al").to_string_lossy());
        state.complete_directory();
        assert_eq!(
            state.path(),
            format!("{}/", temporary.path().join("alpha").display())
        );

        std::fs::create_dir(temporary.path().join("alpine")).unwrap();
        state = New::default();
        state.toggle_mode();
        state.focus_next(); // Path
        type_str(&mut state, &temporary.path().join("al").to_string_lossy());
        state.complete_directory();
        let text = joined(&state);
        assert!(text.contains("Matches:"));
        assert!(text.contains("alpha/"));
        assert!(text.contains("alpine/"));
    }

    #[test]
    fn render_combines_every_section_in_clone_mode() {
        let mut state = New::default();
        state.focus_next(); // Url
        type_str(&mut state, "https://github.com/owner/repo.git");
        state.set_notice(Some("oops".to_string()));
        let text = joined(&state);
        assert!(text.contains("New Project"));
        assert!(text.contains("register an existing directory")); // subtitle
        assert!(text.contains("Repository URL"));
        assert!(text.contains("Location"));
        assert!(text.contains("Directory"));
        assert!(text.contains("Branch"));
        assert!(text.contains("repo")); // derived directory
        assert!(text.contains("Clone"));
        assert!(text.contains("Existing"));
        assert!(text.contains("oops"));
        assert!(text.contains("Esc"));
    }

    #[test]
    fn render_shows_existing_mode_fields() {
        let mut state = New::default();
        state.toggle_mode();
        let text = joined(&state);
        assert!(text.contains("Name"));
        assert!(!text.contains("Repository URL"));
        assert!(!text.contains("Branch"));
    }

    #[test]
    fn render_marks_the_focused_field_with_a_cursor() {
        let mut state = New::default();
        state.focus_next(); // Url focused
        let frame = render(24, 80, &state);
        // フォーカス行に `>` カーソルが 1 つだけ出る（モード選択には出ない）。
        assert_eq!(frame.iter().filter(|l| strip(l).contains('>')).count(), 1);
    }

    #[test]
    fn render_placeholder_shows_on_unfocused_empty_fields() {
        // Url にフォーカスしていない既定状態では、空の Url 欄がプレースホルダを出す。
        let text = joined(&New::default());
        assert!(text.contains("https://github.com/owner/repo.git"));
    }

    #[test]
    fn switching_modes_keeps_the_frame_height_stable() {
        let mut state = New::default();
        let clone = render(24, 80, &state);
        state.toggle_mode();
        let existing = render(24, 80, &state);
        // フィールドブロックを予約高に詰めるので、モード切替でフレーム高は変わらない。
        assert_eq!(clone.len(), existing.len());
    }

    #[test]
    fn notice_slot_keeps_layout_stable_across_toggling() {
        let mut state = New::default();
        let without = render(24, 80, &state);
        state.set_notice(Some("that does not look like a repository URL".to_string()));
        let with = render(24, 80, &state);
        assert_eq!(without.len(), with.len());
    }

    #[test]
    fn render_fills_the_terminal_and_pins_the_footer() {
        let frame = render(40, 80, &New::default());
        assert_eq!(frame.len(), 40);
        assert!(strip(frame.last().unwrap()).contains("Esc"));
        // モード選択にフォーカスのある既定状態では、フッタは種類切替を案内する。
        assert!(strip(frame.last().unwrap()).contains("switch type"));
    }

    #[test]
    fn footer_names_caret_movement_on_a_text_field() {
        let mut state = New::default();
        state.focus_next(); // Url
        let frame = render(40, 80, &state);
        assert!(strip(frame.last().unwrap()).contains("move caret"));
    }

    #[test]
    fn every_field_routes_editing_and_reports_its_caret() {
        // Clone: 各フィールドにフォーカスして編集＆キャレット取得（field_mut / focused_input の
        // 全バリアントを通す）。
        let mut clone = New::default();
        for _ in 0..4 {
            // Mode -> Url -> Location -> Directory -> Branch
            clone.focus_next();
            clone.insert_char('x');
            let _ = clone.focus_cursor();
        }
        assert_eq!(clone.url(), "x");
        assert_eq!(clone.location(), "x");
        assert!(clone.branch().contains('x'));

        // Existing: Path / Name。
        let mut existing = New::default();
        existing.toggle_mode();
        existing.focus_next(); // Path
        existing.insert_char('p');
        let _ = existing.focus_cursor();
        existing.focus_next(); // Name
        existing.insert_char('n');
        let _ = existing.focus_cursor();
        assert!(existing.path().contains('p'));
        assert!(existing.name().contains('n'));
    }

    #[test]
    fn render_draws_the_caret_in_the_middle_of_a_value() {
        // キャレットが末尾でない（後続文字がある）フォーカス入力を描く。
        let mut state = New::default();
        state.focus_next(); // Url
        type_str(&mut state, "abc");
        state.cursor_left(); // キャレットは 'c' の手前
        let text = joined(&state);
        assert!(text.contains("abc"));
        assert!(
            render(24, 80, &state)
                .join("\n")
                .contains("\u{1b}[1;7;36mc\u{1b}[0m")
        );
    }
}

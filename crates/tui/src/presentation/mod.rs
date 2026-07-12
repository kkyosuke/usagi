//! TUI 面の presentation 層。画面描画（各画面の view・共通 widget）と
//! キー入力のマッピングを置く。描画は v1 と同じく自前の差分レンダリングで行い、
//! UI フレームワーク（ratatui 等）には依存しない方針を引き継ぐ。
//! 実 IO は持たず、出力先は呼び出し側（合成ルート）から注入する。
//!
//! 描画は 3 つに分ける: 各画面の view（[`views`]）・再利用 UI 部品（[`widgets`]）・
//! 領域配置（[`layouts`]）。view が layout で領域を割り、そこへ widget を配置する。
//! 色は [`theme`] が意味的な役割で一元管理する（役割→具体色の単一情報源）。

pub mod layouts;
pub mod theme;
pub mod views;
pub mod widgets;

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use usagi_core::domain::AppInfo;
use usagi_core::domain::workspace::Workspace;

use crate::presentation::views::new::{self, Field, New};
use crate::presentation::views::open::{self, Open};
use crate::presentation::views::welcome::{self, MenuAction, Welcome};
use crate::usecase::application::{Key, ScreenRunner, Terminal};

/// 起動バナーを `out` に書き出す。
///
/// # Errors
///
/// `out` への書き込みに失敗した場合、そのエラーを返す。
pub fn write_banner(out: &mut impl Write, info: &AppInfo) -> std::io::Result<()> {
    writeln!(out, "{}", info.describe())
}

/// 対話ループが終了する理由。合成ルートがこれを見て後続の動作を決める。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    /// ユーザーが終了した（`q` / Ctrl-C、または welcome で Esc）。
    Quit,
    /// ユーザーが選んだ workspace を開く。合成ルートが workspace 画面へ接続する。
    OpenWorkspace(PathBuf),
}

/// いま表示している画面。
enum Screen {
    Welcome,
    Open,
    New,
}

/// welcome 画面でキー `key` を処理した結果の遷移。
enum WelcomeStep {
    /// 同じ画面に留まる。
    Stay,
    /// 終了する。
    Quit,
    /// Open（workspace 一覧）へ進む。
    OpenList,
    /// New（新規 workspace 作成フォーム）へ進む。
    NewForm,
}

/// New 画面でキー `key` を処理した結果の遷移。
enum NewStep {
    /// 同じ画面に留まる（フォーム編集を続ける）。
    Stay,
    /// 終了する。
    Quit,
    /// welcome へ戻る。
    Back,
}

/// Open 画面でキー `key` を処理した結果の遷移。
enum OpenStep {
    /// 同じ画面に留まる。
    Stay,
    /// 終了する。
    Quit,
    /// welcome へ戻る。
    Back,
    /// 選んだ workspace を開く。
    Choose(PathBuf),
}

/// welcome のメニュー操作 `action` を画面遷移へ写す。遷移先が未実装の項目（Config /
/// recent）は同じ画面に留まる。
fn welcome_action(action: MenuAction) -> WelcomeStep {
    match action {
        MenuAction::Quit => WelcomeStep::Quit,
        MenuAction::Open => WelcomeStep::OpenList,
        MenuAction::New => WelcomeStep::NewForm,
        MenuAction::Config | MenuAction::OpenRecent(_) => WelcomeStep::Stay,
    }
}

/// welcome 画面のキー処理（純粋）。上下で選択を動かし、確定・ショートカットを遷移へ写す。
/// 最上位画面なので Esc も終了として扱う。
fn step_welcome(welcome: &mut Welcome, key: Key) -> WelcomeStep {
    match key {
        Key::Up => {
            welcome.select_prev();
            WelcomeStep::Stay
        }
        Key::Down => {
            welcome.select_next();
            WelcomeStep::Stay
        }
        Key::Escape | Key::Quit => WelcomeStep::Quit,
        Key::Enter => welcome_action(welcome.selected_action()),
        Key::Char(ch) => welcome
            .action_for(ch)
            .map_or(WelcomeStep::Stay, welcome_action),
        Key::Left | Key::Right | Key::Backspace | Key::Other => WelcomeStep::Stay,
    }
}

/// New 画面のキー処理（純粋）。上下でフィールドを移り、←→ でモード切替（モード選択時）または
/// キャレット移動、文字入力・Backspace で編集、Esc で welcome へ戻り、`Ctrl-C` で終了する。
/// フォームの確定（作成）は作成処理が入るまで留まる。
fn step_new(form: &mut New, key: Key) -> NewStep {
    match key {
        Key::Up => {
            form.focus_prev();
            NewStep::Stay
        }
        Key::Down => {
            form.focus_next();
            NewStep::Stay
        }
        Key::Left => {
            step_new_horizontal(form, false);
            NewStep::Stay
        }
        Key::Right => {
            step_new_horizontal(form, true);
            NewStep::Stay
        }
        Key::Backspace => {
            form.backspace();
            NewStep::Stay
        }
        Key::Char(ch) => {
            form.insert_char(ch);
            NewStep::Stay
        }
        Key::Escape => NewStep::Back,
        Key::Quit => NewStep::Quit,
        Key::Enter | Key::Other => NewStep::Stay,
    }
}

/// New 画面の ←→ 操作。モード選択にフォーカスがあるときはモードを切り替え、テキスト欄では
/// キャレットを左右へ動かす（`right` が右方向）。
fn step_new_horizontal(form: &mut New, right: bool) {
    if form.focus() == Field::Mode {
        form.toggle_mode();
    } else if right {
        form.cursor_right();
    } else {
        form.cursor_left();
    }
}

/// Open 画面のキー処理（純粋）。上下で選択を動かし、Enter で開く workspace を確定、
/// Esc で welcome へ戻り、`q` / Ctrl-C で終了する。
fn step_open(open: &mut Open, key: Key) -> OpenStep {
    match key {
        Key::Up => {
            open.select_prev();
            OpenStep::Stay
        }
        Key::Down => {
            open.select_next();
            OpenStep::Stay
        }
        Key::Escape => OpenStep::Back,
        Key::Quit | Key::Char('q') => OpenStep::Quit,
        Key::Enter => open.selected().map_or(OpenStep::Stay, |workspace| {
            OpenStep::Choose(workspace.path.clone())
        }),
        Key::Char(_) | Key::Left | Key::Right | Key::Backspace | Key::Other => OpenStep::Stay,
    }
}

/// welcome を起点にした対話ループ。毎フレーム現在の画面を端末サイズで描き、キーを 1 つ読んで
/// 画面遷移を進める。welcome の Open から workspace 一覧（Open 画面）へ入り、そこで選んだ
/// workspace を [`Exit::OpenWorkspace`] として返す。終了操作では [`Exit::Quit`] を返す。
///
/// `workspaces` は登録済み workspace の一覧（読み出しは実 IO なので合成ルートが渡す）。`now` は
/// 相対時刻に使う（この層は実時計を読まない）。実端末の制御は注入された [`Terminal`] に委ね、
/// この関数は「何を描き、キーをどう解釈して遷移するか」だけを持つ純粋な制御ループである。
///
/// # Errors
///
/// 端末への描画またはキー読み取りに失敗した場合、そのエラーを返す。
pub fn run(
    term: &mut dyn Terminal,
    workspaces: Vec<Workspace>,
    now: DateTime<Utc>,
) -> io::Result<Exit> {
    let mut welcome = Welcome::empty();
    let mut open = Open::new(workspaces);
    let mut new_form = New::default();
    let mut screen = Screen::Welcome;
    loop {
        let (height, width) = term.size()?;
        let frame = match screen {
            Screen::Welcome => welcome::render(height, width, &welcome, now),
            Screen::Open => open::render(height, width, &open, now),
            Screen::New => new::render(height, width, &new_form),
        };
        term.draw(&frame)?;
        let key = term.read_key()?;
        match screen {
            Screen::Welcome => match step_welcome(&mut welcome, key) {
                WelcomeStep::Stay => {}
                WelcomeStep::Quit => return Ok(Exit::Quit),
                WelcomeStep::OpenList => screen = Screen::Open,
                WelcomeStep::NewForm => screen = Screen::New,
            },
            Screen::Open => match step_open(&mut open, key) {
                OpenStep::Stay => {}
                OpenStep::Quit => return Ok(Exit::Quit),
                OpenStep::Back => screen = Screen::Welcome,
                OpenStep::Choose(path) => return Ok(Exit::OpenWorkspace(path)),
            },
            Screen::New => match step_new(&mut new_form, key) {
                NewStep::Stay => {}
                NewStep::Quit => return Ok(Exit::Quit),
                NewStep::Back => screen = Screen::Welcome,
            },
        }
    }
}

/// 端末 runtime が実装されるまで、選ばれた TUI 画面を識別できる一行を出力する暫定 runner。
///
/// 出力先とアプリ情報は呼び出し側から注入するため、実 stdout を直接所有しない。
pub struct BannerScreenRunner<'a, W: Write + ?Sized> {
    out: &'a mut W,
    info: &'a AppInfo,
}

impl<'a, W: Write + ?Sized> BannerScreenRunner<'a, W> {
    /// 注入された出力先とアプリ情報から runner を作る。
    #[must_use]
    pub fn new(out: &'a mut W, info: &'a AppInfo) -> Self {
        Self { out, info }
    }

    /// 画面を識別する `label` をアプリ情報とともに一行で書き出す。
    fn write_screen(&mut self, label: &str) -> io::Result<()> {
        writeln!(self.out, "{}: {label}", self.info.describe())
    }
}

impl<W: Write + ?Sized> ScreenRunner for BannerScreenRunner<'_, W> {
    fn welcome(&mut self) -> io::Result<()> {
        self.write_screen("welcome TUI")
    }

    fn workspace(&mut self, path: &Path) -> io::Result<()> {
        self.write_screen(&format!("workspace TUI ({})", path.display()))
    }

    fn config(&mut self) -> io::Result<()> {
        self.write_screen("config TUI")
    }

    fn doctor(&mut self) -> io::Result<()> {
        self.write_screen("doctor TUI")
    }
}

#[cfg(test)]
mod tests {
    use super::{BannerScreenRunner, Exit, NewStep, run, step_new, write_banner};
    use crate::presentation::views::new::{Field, Mode, New};
    use crate::usecase::application::run as dispatch;
    use crate::usecase::application::{EntryScreen, Key, Terminal};
    use chrono::{DateTime, Utc};
    use std::collections::VecDeque;
    use std::io::{self, Write};
    use std::path::PathBuf;
    use usagi_core::domain::AppInfo;
    use usagi_core::domain::workspace::Workspace;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-06-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn ws(name: &str) -> Workspace {
        Workspace::new(name, format!("/tmp/{name}"))
    }

    /// テスト用の [`Terminal`]。あらかじめ積んだキー列を順に返し、描いたフレームを記録する。
    /// `fail_size` / `fail_draw` でサイズ取得・描画のエラーを模す。キーが尽きると
    /// `read_key` はエラーになるので、各テストは終了キーで締めてループを止める。
    #[derive(Default)]
    struct FakeTerminal {
        keys: VecDeque<Key>,
        frames: Vec<Vec<String>>,
        fail_size: bool,
        fail_draw: bool,
    }

    impl FakeTerminal {
        fn with_keys(keys: &[Key]) -> Self {
            Self {
                keys: keys.iter().copied().collect(),
                ..Self::default()
            }
        }
    }

    impl Terminal for FakeTerminal {
        fn size(&mut self) -> io::Result<(usize, usize)> {
            if self.fail_size {
                return Err(io::Error::other("size failed"));
            }
            // 0 は welcome::render 側で 24x80 にフォールバックされる。
            Ok((0, 0))
        }

        fn draw(&mut self, frame: &[String]) -> io::Result<()> {
            if self.fail_draw {
                return Err(io::Error::other("draw failed"));
            }
            self.frames.push(frame.to_vec());
            Ok(())
        }

        fn read_key(&mut self) -> io::Result<Key> {
            self.keys
                .pop_front()
                .ok_or_else(|| io::Error::other("no more keys"))
        }
    }

    #[test]
    fn run_quits_on_the_quit_shortcut() {
        let mut term = FakeTerminal::with_keys(&[Key::Char('q')]);
        assert_eq!(run(&mut term, Vec::new(), now()).unwrap(), Exit::Quit);
        // 1 度描いてから q で抜ける。最初のフレームは welcome 画面。
        assert_eq!(term.frames.len(), 1);
        let joined = term.frames[0].join("\n");
        assert!(joined.contains("USAGI"));
        assert!(joined.contains("Menu"));
    }

    #[test]
    fn run_quits_on_ctrl_c_and_on_escape_at_welcome() {
        for quit_key in [Key::Quit, Key::Escape] {
            let mut term = FakeTerminal::with_keys(&[quit_key]);
            assert_eq!(run(&mut term, Vec::new(), now()).unwrap(), Exit::Quit);
            assert_eq!(term.frames.len(), 1);
        }
    }

    #[test]
    fn run_navigates_welcome_before_quitting() {
        let mut term = FakeTerminal::with_keys(&[Key::Down, Key::Down, Key::Up, Key::Quit]);
        assert_eq!(run(&mut term, Vec::new(), now()).unwrap(), Exit::Quit);
        // 各キーの前に 1 度ずつ描くので 4 フレーム。
        assert_eq!(term.frames.len(), 4);
    }

    #[test]
    fn run_stays_on_welcome_for_unwired_items_and_unknown_keys() {
        // 'c'(Config) と未知の 'z'、Other は welcome に留まり、q で抜ける。
        let mut term =
            FakeTerminal::with_keys(&[Key::Char('c'), Key::Char('z'), Key::Other, Key::Char('q')]);
        assert_eq!(run(&mut term, Vec::new(), now()).unwrap(), Exit::Quit);
        assert_eq!(term.frames.len(), 4);
        // どのフレームも welcome のまま（Open / New へは行かない）。
        assert!(term.frames.iter().all(|f| f.join("\n").contains("Menu")));
    }

    #[test]
    fn run_enters_the_new_form_from_welcome_and_returns() {
        // 'e'(New) で New フォームへ、Esc で welcome へ戻り、q で終了する。
        let mut term = FakeTerminal::with_keys(&[Key::Char('e'), Key::Escape, Key::Char('q')]);
        assert_eq!(run(&mut term, Vec::new(), now()).unwrap(), Exit::Quit);
        assert_eq!(term.frames.len(), 3);
        assert!(term.frames[0].join("\n").contains("Menu")); // welcome
        assert!(term.frames[1].join("\n").contains("New Project")); // New フォーム
        assert!(term.frames[2].join("\n").contains("Menu")); // welcome へ戻る
    }

    #[test]
    fn run_stays_on_the_new_form_while_editing_then_quits() {
        // New へ入り、フォーム内でフィールド移動（留まる）してから終了（Ctrl-C 相当）する。
        let mut term = FakeTerminal::with_keys(&[Key::Char('e'), Key::Down, Key::Quit]);
        assert_eq!(run(&mut term, Vec::new(), now()).unwrap(), Exit::Quit);
        assert_eq!(term.frames.len(), 3);
        // New に入ったあと、編集操作の間も New に留まる。
        assert!(term.frames[1].join("\n").contains("New Project"));
        assert!(term.frames[2].join("\n").contains("New Project"));
    }

    #[test]
    fn step_new_edits_navigates_and_returns_back_or_quits() {
        let mut form = New::default();
        // 上下でフィールド移動。
        assert!(matches!(step_new(&mut form, Key::Down), NewStep::Stay));
        assert_eq!(form.focus(), Field::Url);
        assert!(matches!(step_new(&mut form, Key::Up), NewStep::Stay));
        assert_eq!(form.focus(), Field::Mode);
        // モード選択では ←→ でモードを切り替える。
        step_new(&mut form, Key::Right);
        assert_eq!(form.mode(), Mode::Existing);
        step_new(&mut form, Key::Left);
        assert_eq!(form.mode(), Mode::Clone);
        // テキスト欄では入力・Backspace・←→ キャレット移動。
        step_new(&mut form, Key::Down); // Url
        step_new(&mut form, Key::Char('a'));
        step_new(&mut form, Key::Char('b'));
        assert_eq!(form.url(), "ab");
        step_new(&mut form, Key::Left); // キャレットを a|b の間へ
        step_new(&mut form, Key::Right); // 末尾へ戻す
        assert!(matches!(step_new(&mut form, Key::Backspace), NewStep::Stay));
        assert_eq!(form.url(), "a");
        // Enter / Other は留まる。
        assert!(matches!(step_new(&mut form, Key::Enter), NewStep::Stay));
        assert!(matches!(step_new(&mut form, Key::Other), NewStep::Stay));
        // Esc は welcome へ戻り、Quit は終了。
        assert!(matches!(step_new(&mut form, Key::Escape), NewStep::Back));
        assert!(matches!(step_new(&mut form, Key::Quit), NewStep::Quit));
    }

    #[test]
    fn run_quits_when_the_quit_item_is_confirmed() {
        // Quit 項目（末尾）まで移動してから Enter で抜ける。
        let mut term = FakeTerminal::with_keys(&[Key::Down, Key::Down, Key::Down, Key::Enter]);
        assert_eq!(run(&mut term, Vec::new(), now()).unwrap(), Exit::Quit);
        assert_eq!(term.frames.len(), 4);
    }

    #[test]
    fn run_enters_the_open_list_from_welcome() {
        // 先頭項目 Open を Enter で確定すると Open 画面へ進む。
        let mut term = FakeTerminal::with_keys(&[Key::Enter, Key::Quit]);
        assert_eq!(
            run(&mut term, vec![ws("alpha")], now()).unwrap(),
            Exit::Quit
        );
        assert_eq!(term.frames.len(), 2);
        // frame0 は welcome、frame1 は Open 画面（workspace 一覧）。
        assert!(term.frames[0].join("\n").contains("Menu"));
        assert!(term.frames[1].join("\n").contains("Open Workspace"));
        assert!(term.frames[1].join("\n").contains("alpha"));
    }

    #[test]
    fn run_enters_the_open_list_with_the_o_shortcut() {
        let mut term = FakeTerminal::with_keys(&[Key::Char('o'), Key::Quit]);
        assert_eq!(
            run(&mut term, vec![ws("alpha")], now()).unwrap(),
            Exit::Quit
        );
        assert!(term.frames[1].join("\n").contains("Workspaces"));
    }

    #[test]
    fn run_returns_the_workspace_chosen_in_the_open_list() {
        // Open へ入り、Enter で選択中（先頭 alpha）の workspace を開く。
        let mut term = FakeTerminal::with_keys(&[Key::Char('o'), Key::Enter]);
        let exit = run(&mut term, vec![ws("alpha"), ws("beta")], now()).unwrap();
        assert_eq!(exit, Exit::OpenWorkspace(PathBuf::from("/tmp/alpha")));
    }

    #[test]
    fn run_open_list_navigates_before_choosing() {
        // Down で beta に移り、Enter で beta を開く。
        let mut term = FakeTerminal::with_keys(&[Key::Char('o'), Key::Down, Key::Enter]);
        let exit = run(&mut term, vec![ws("alpha"), ws("beta")], now()).unwrap();
        assert_eq!(exit, Exit::OpenWorkspace(PathBuf::from("/tmp/beta")));
    }

    #[test]
    fn run_open_list_prev_wraps_to_the_last_workspace() {
        // Up で末尾 beta に回り込み、Enter で beta を開く。
        let mut term = FakeTerminal::with_keys(&[Key::Char('o'), Key::Up, Key::Enter]);
        let exit = run(&mut term, vec![ws("alpha"), ws("beta")], now()).unwrap();
        assert_eq!(exit, Exit::OpenWorkspace(PathBuf::from("/tmp/beta")));
    }

    #[test]
    fn run_open_list_returns_to_welcome_on_escape() {
        // Open へ入り、Esc で welcome に戻ってから終了する。
        let mut term = FakeTerminal::with_keys(&[Key::Char('o'), Key::Escape, Key::Quit]);
        assert_eq!(
            run(&mut term, vec![ws("alpha")], now()).unwrap(),
            Exit::Quit
        );
        assert_eq!(term.frames.len(), 3);
        assert!(term.frames[1].join("\n").contains("Open Workspace"));
        // Esc 後は welcome に戻る。
        assert!(term.frames[2].join("\n").contains("Menu"));
    }

    #[test]
    fn run_open_list_quits_on_q_and_ignores_other_keys() {
        // Open で未知の 'z' と Other は留まり、'q' で終了する。
        let mut term =
            FakeTerminal::with_keys(&[Key::Char('o'), Key::Char('z'), Key::Other, Key::Char('q')]);
        assert_eq!(
            run(&mut term, vec![ws("alpha")], now()).unwrap(),
            Exit::Quit
        );
        assert_eq!(term.frames.len(), 4);
    }

    #[test]
    fn run_open_list_enter_on_an_empty_list_stays() {
        // workspace が無いと Enter では開けず留まり、Esc で戻って終了する。
        let mut term =
            FakeTerminal::with_keys(&[Key::Char('o'), Key::Enter, Key::Escape, Key::Quit]);
        assert_eq!(run(&mut term, Vec::new(), now()).unwrap(), Exit::Quit);
        assert_eq!(term.frames.len(), 4);
        assert!(term.frames[1].join("\n").contains("No workspaces yet"));
    }

    #[test]
    fn run_propagates_a_size_failure() {
        let mut term = FakeTerminal {
            fail_size: true,
            ..FakeTerminal::default()
        };
        let error = run(&mut term, Vec::new(), now()).unwrap_err();
        assert_eq!(error.to_string(), "size failed");
    }

    #[test]
    fn run_propagates_a_draw_failure() {
        let mut term = FakeTerminal {
            fail_draw: true,
            ..FakeTerminal::default()
        };
        let error = run(&mut term, Vec::new(), now()).unwrap_err();
        assert_eq!(error.to_string(), "draw failed");
    }

    #[test]
    fn run_propagates_a_read_failure() {
        // キーを積まないと read_key がエラーになる（入力読み取りの失敗を模す）。
        let mut term = FakeTerminal::default();
        let error = run(&mut term, Vec::new(), now()).unwrap_err();
        assert_eq!(error.to_string(), "no more keys");
    }

    #[test]
    fn exit_derives_are_exercised() {
        let quit = Exit::Quit;
        assert_eq!(quit.clone(), Exit::Quit);
        assert!(format!("{quit:?}").contains("Quit"));
        let open = Exit::OpenWorkspace(PathBuf::from("/tmp/x"));
        assert_eq!(open.clone(), open);
        assert!(format!("{open:?}").contains("OpenWorkspace"));
        assert_ne!(quit, open);
    }

    fn info() -> AppInfo {
        AppInfo {
            name: "usagi",
            version: "0.1.0",
        }
    }

    #[test]
    fn write_banner_writes_description_line() {
        let mut buf = Vec::new();
        write_banner(&mut buf, &info()).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "usagi v0.1.0\n");
    }

    #[test]
    fn banner_screen_runner_names_every_tui_screen() {
        let entries = [
            EntryScreen::Welcome,
            EntryScreen::Workspace {
                path: PathBuf::from("/tmp/project"),
            },
            EntryScreen::Config,
            EntryScreen::Doctor,
        ];
        let mut buf = Vec::new();
        let info = info();
        let mut runner = BannerScreenRunner::new(&mut buf, &info);

        for entry in &entries {
            dispatch(entry, &mut runner).unwrap();
        }

        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "usagi v0.1.0: welcome TUI\n\
             usagi v0.1.0: workspace TUI (/tmp/project)\n\
             usagi v0.1.0: config TUI\n\
             usagi v0.1.0: doctor TUI\n"
        );
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("write failed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn banner_screen_runner_propagates_write_failure() {
        let mut out = FailingWriter;
        out.flush().unwrap();
        let info = info();
        let mut runner = BannerScreenRunner::new(&mut out, &info);

        let error = dispatch(&EntryScreen::Welcome, &mut runner).unwrap_err();

        assert_eq!(error.to_string(), "write failed");
    }
}

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
use std::path::Path;

use chrono::{DateTime, Utc};
use usagi_core::domain::AppInfo;

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

/// 対話的な welcome 画面を回す。毎フレーム現在の端末サイズで [`welcome::render`] を描き、
/// キーを 1 つ読んで状態を更新する。終了操作（`q` / Esc / Ctrl-C、または `Quit` 項目の確定）で
/// ループを抜けて戻る。`now` は recent カードの相対時刻に使うので合成ルートが実時計から渡す
/// （この層は時計を読まない）。実端末の制御は注入された [`Terminal`] に委ね、この関数は
/// 「何を描き、キーをどう解釈するか」だけを持つ純粋な制御ループである。
///
/// # Errors
///
/// 端末への描画またはキー読み取りに失敗した場合、そのエラーを返す。
pub fn run_welcome(
    term: &mut dyn Terminal,
    welcome: &mut Welcome,
    now: DateTime<Utc>,
) -> io::Result<()> {
    loop {
        let (height, width) = term.size()?;
        term.draw(&welcome::render(height, width, welcome, now))?;
        match term.read_key()? {
            Key::Up => welcome.select_prev(),
            Key::Down => welcome.select_next(),
            Key::Quit => return Ok(()),
            Key::Enter => {
                if welcome.selected_action() == MenuAction::Quit {
                    return Ok(());
                }
            }
            Key::Char(ch) => {
                if welcome.action_for(ch) == Some(MenuAction::Quit) {
                    return Ok(());
                }
            }
            Key::Other => {}
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
    use super::{BannerScreenRunner, run_welcome, write_banner};
    use crate::presentation::views::welcome::Welcome;
    use crate::usecase::application::{EntryScreen, Key, Terminal, run};
    use chrono::{DateTime, Utc};
    use std::collections::VecDeque;
    use std::io::{self, Write};
    use std::path::PathBuf;
    use usagi_core::domain::AppInfo;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-06-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
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
    fn run_welcome_quits_on_the_quit_shortcut() {
        let mut term = FakeTerminal::with_keys(&[Key::Char('q')]);
        let mut welcome = Welcome::empty();
        run_welcome(&mut term, &mut welcome, now()).unwrap();
        // 1 度描いてから q で抜ける。描いたフレームは welcome 画面。
        assert_eq!(term.frames.len(), 1);
        let joined = term.frames[0].join("\n");
        assert!(joined.contains("USAGI"));
        assert!(joined.contains("Menu"));
    }

    #[test]
    fn run_welcome_quits_on_the_quit_key() {
        let mut term = FakeTerminal::with_keys(&[Key::Quit]);
        let mut welcome = Welcome::empty();
        run_welcome(&mut term, &mut welcome, now()).unwrap();
        assert_eq!(term.frames.len(), 1);
    }

    #[test]
    fn run_welcome_navigates_before_quitting() {
        // Down で New(1)、さらに Down で Config(2)、Up で New(1) に戻り、Quit で抜ける。
        let mut term = FakeTerminal::with_keys(&[Key::Down, Key::Down, Key::Up, Key::Quit]);
        let mut welcome = Welcome::empty();
        run_welcome(&mut term, &mut welcome, now()).unwrap();
        assert_eq!(welcome.selected_index(), 1);
        // 各キーの前に 1 度ずつ描くので 4 フレーム。
        assert_eq!(term.frames.len(), 4);
    }

    #[test]
    fn run_welcome_enter_on_a_non_quit_item_stays() {
        // 先頭は Open。Enter では抜けず、続く Quit で抜ける（描画は 2 回）。
        let mut term = FakeTerminal::with_keys(&[Key::Enter, Key::Quit]);
        let mut welcome = Welcome::empty();
        run_welcome(&mut term, &mut welcome, now()).unwrap();
        assert_eq!(term.frames.len(), 2);
    }

    #[test]
    fn run_welcome_enter_on_the_quit_item_exits() {
        // Quit 項目（末尾）まで移動してから Enter で抜ける。
        let mut term = FakeTerminal::with_keys(&[Key::Down, Key::Down, Key::Down, Key::Enter]);
        let mut welcome = Welcome::empty();
        run_welcome(&mut term, &mut welcome, now()).unwrap();
        assert_eq!(welcome.selected_index(), 3);
        assert_eq!(term.frames.len(), 4);
    }

    #[test]
    fn run_welcome_ignores_non_quit_shortcuts_and_unknown_keys() {
        // 'o'(Open) と未知の 'z'、Other は無視して回り続け、q で抜ける。
        let mut term =
            FakeTerminal::with_keys(&[Key::Char('o'), Key::Char('z'), Key::Other, Key::Char('q')]);
        let mut welcome = Welcome::empty();
        run_welcome(&mut term, &mut welcome, now()).unwrap();
        assert_eq!(term.frames.len(), 4);
    }

    #[test]
    fn run_welcome_propagates_a_size_failure() {
        let mut term = FakeTerminal {
            fail_size: true,
            ..FakeTerminal::default()
        };
        let mut welcome = Welcome::empty();
        let error = run_welcome(&mut term, &mut welcome, now()).unwrap_err();
        assert_eq!(error.to_string(), "size failed");
    }

    #[test]
    fn run_welcome_propagates_a_draw_failure() {
        let mut term = FakeTerminal {
            fail_draw: true,
            ..FakeTerminal::default()
        };
        let mut welcome = Welcome::empty();
        let error = run_welcome(&mut term, &mut welcome, now()).unwrap_err();
        assert_eq!(error.to_string(), "draw failed");
    }

    #[test]
    fn run_welcome_propagates_a_read_failure() {
        // キーを積まないと read_key がエラーになる（入力読み取りの失敗を模す）。
        let mut term = FakeTerminal::default();
        let mut welcome = Welcome::empty();
        let error = run_welcome(&mut term, &mut welcome, now()).unwrap_err();
        assert_eq!(error.to_string(), "no more keys");
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
            run(entry, &mut runner).unwrap();
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

        let error = run(&EntryScreen::Welcome, &mut runner).unwrap_err();

        assert_eq!(error.to_string(), "write failed");
    }
}

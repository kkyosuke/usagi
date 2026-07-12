//! TUI の起動画面を選び、対応する画面 runner へ委譲する application 境界。
//!
//! CLI 面は TUI クレートへ依存できないため、CLI が要求した画面への変換は合成ルートが
//! 行う。このモジュールは変換後の [`EntryScreen`] を受け取り、画面の具体的な描画・入力
//! 処理を [`ScreenRunner`] へ委譲する。これにより画面遷移の判断を端末 IO から分離する。

use std::io;
use std::path::{Path, PathBuf};

/// TUI をどの画面から開始するかを表す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryScreen {
    /// トップメニューから開始する。
    Welcome,
    /// 指定された workspace の画面から開始する。
    Workspace {
        /// 開く workspace のパス。合成ルートで解決済みの値を受け取る。
        path: PathBuf,
    },
    /// グローバル設定画面から開始する。
    Config,
    /// 必要ツールの診断画面から開始する。
    Doctor,
}

/// 各起動画面を実行する presentation 側の境界。
///
/// 実端末の制御やキー入力は実装側へ注入し、application 層は具体的な IO を持たない。
pub trait ScreenRunner {
    /// Welcome 画面を実行する。
    ///
    /// # Errors
    ///
    /// 画面の描画または入力処理に失敗した場合、そのエラーを返す。
    fn welcome(&mut self) -> io::Result<()>;

    /// `path` の workspace 画面を実行する。
    ///
    /// # Errors
    ///
    /// 画面の描画または入力処理に失敗した場合、そのエラーを返す。
    fn workspace(&mut self, path: &Path) -> io::Result<()>;

    /// Config 画面を実行する。
    ///
    /// # Errors
    ///
    /// 画面の描画または入力処理に失敗した場合、そのエラーを返す。
    fn config(&mut self) -> io::Result<()>;

    /// Doctor 画面を実行する。
    ///
    /// # Errors
    ///
    /// 画面の描画または入力処理に失敗した場合、そのエラーを返す。
    fn doctor(&mut self) -> io::Result<()>;
}

/// 対話画面が端末から受け取る 1 つのキー入力。実端末のイベント（crossterm など）は
/// 合成ルートがこの語彙へ翻訳して渡すため、TUI 面は特定の端末ライブラリに依存しない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// 選択を 1 つ上へ移す。
    Up,
    /// 選択を 1 つ下へ移す。
    Down,
    /// 選択中の項目を確定する。
    Enter,
    /// 画面を終了する（Esc / Ctrl-C など）。
    Quit,
    /// 文字キー。メニューのショートカット文字や recent の番号キーに使う。
    Char(char),
    /// 上記のいずれでもないキー（無視して再描画だけする。リサイズ通知など）。
    Other,
}

/// 対話画面が使う端末の最小インターフェース。実端末の制御（raw mode・画面描画・
/// キー読み取り）は合成ルートが実装して注入し、この層は注入された `Terminal` に対して
/// 純粋に振る舞う（描くフレームの構築とキーの解釈だけを担う）。
pub trait Terminal {
    /// 現在の端末サイズ `(height, width)`（行数・桁数）を返す。
    ///
    /// # Errors
    ///
    /// 端末サイズの取得に失敗した場合、そのエラーを返す。
    fn size(&mut self) -> io::Result<(usize, usize)>;

    /// 1 フレーム分の行を端末へ描く。
    ///
    /// # Errors
    ///
    /// 端末への書き込みに失敗した場合、そのエラーを返す。
    fn draw(&mut self, frame: &[String]) -> io::Result<()>;

    /// 次のキー入力を 1 つ読む（入力があるまでブロックする）。
    ///
    /// # Errors
    ///
    /// キー入力の読み取りに失敗した場合、そのエラーを返す。
    fn read_key(&mut self) -> io::Result<Key>;
}

/// `entry` に対応する画面を `runner` で実行する。
///
/// # Errors
///
/// 選ばれた画面の runner が失敗した場合、そのエラーをそのまま返す。
pub fn run(entry: &EntryScreen, runner: &mut dyn ScreenRunner) -> io::Result<()> {
    match entry {
        EntryScreen::Welcome => runner.welcome(),
        EntryScreen::Workspace { path } => runner.workspace(path),
        EntryScreen::Config => runner.config(),
        EntryScreen::Doctor => runner.doctor(),
    }
}

#[cfg(test)]
mod tests {
    use super::{EntryScreen, ScreenRunner, run};
    use std::io;
    use std::path::{Path, PathBuf};

    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Welcome,
        Workspace(PathBuf),
        Config,
        Doctor,
    }

    #[derive(Default)]
    struct RecordingRunner {
        calls: Vec<Call>,
        fail_doctor: bool,
    }

    impl ScreenRunner for RecordingRunner {
        fn welcome(&mut self) -> io::Result<()> {
            self.calls.push(Call::Welcome);
            Ok(())
        }

        fn workspace(&mut self, path: &Path) -> io::Result<()> {
            self.calls.push(Call::Workspace(path.to_path_buf()));
            Ok(())
        }

        fn config(&mut self) -> io::Result<()> {
            self.calls.push(Call::Config);
            Ok(())
        }

        fn doctor(&mut self) -> io::Result<()> {
            self.calls.push(Call::Doctor);
            if self.fail_doctor {
                Err(io::Error::other("doctor failed"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn dispatches_every_entry_screen() {
        let entries = [
            EntryScreen::Welcome,
            EntryScreen::Workspace {
                path: PathBuf::from("/tmp/project"),
            },
            EntryScreen::Config,
            EntryScreen::Doctor,
        ];
        let mut runner = RecordingRunner::default();

        for entry in &entries {
            run(entry, &mut runner).unwrap();
        }

        assert_eq!(
            runner.calls,
            vec![
                Call::Welcome,
                Call::Workspace(PathBuf::from("/tmp/project")),
                Call::Config,
                Call::Doctor,
            ]
        );
    }

    #[test]
    fn entry_screen_derives_clone_equality_and_debug() {
        let entries = [
            EntryScreen::Welcome,
            EntryScreen::Workspace {
                path: PathBuf::from("/tmp/project"),
            },
            EntryScreen::Config,
            EntryScreen::Doctor,
        ];

        for entry in entries {
            assert_eq!(entry.clone(), entry);
            assert!(!format!("{entry:?}").is_empty());
        }
    }

    #[test]
    fn key_derives_are_exercised() {
        use super::Key;
        // derive された Debug / Clone / Copy / PartialEq を全バリアントで実行する。
        let keys = [
            Key::Up,
            Key::Down,
            Key::Enter,
            Key::Quit,
            Key::Char('o'),
            Key::Other,
        ];
        for key in keys {
            assert_eq!(key, key);
            assert!(!format!("{key:?}").is_empty());
        }
        assert_ne!(Key::Char('a'), Key::Char('b'));
    }

    #[test]
    fn propagates_screen_runner_errors() {
        let mut runner = RecordingRunner {
            fail_doctor: true,
            ..RecordingRunner::default()
        };

        let error = run(&EntryScreen::Doctor, &mut runner).unwrap_err();

        assert_eq!(error.to_string(), "doctor failed");
        assert_eq!(runner.calls, vec![Call::Doctor]);
    }
}

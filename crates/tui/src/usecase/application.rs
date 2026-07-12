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

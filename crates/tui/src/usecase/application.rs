//! TUI の起動画面を選び、対応する画面 runner へ委譲する application 境界。
//!
//! CLI 面は TUI クレートへ依存できないため、CLI が要求した画面への変換は合成ルートが
//! 行う。このモジュールは変換後の [`EntryScreen`] を受け取り、画面の具体的な描画・入力
//! 処理を [`ScreenRunner`] へ委譲する。これにより画面遷移の判断を端末 IO から分離する。

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use usagi_core::domain::id::{SessionId, WorkspaceId};
use usagi_core::domain::workspace::Workspace;
use usagi_core::domain::workspace_state::WorkspaceState;

/// Daemon-authoritative Agent launch adapter for Closeup panes.
pub mod agent_launch;
/// v2 controller effect と daemon-owned Agent pane runtime を結合する host。
pub mod agent_runtime;
/// Home の application controller。端末や daemon wire 型に依存しない reducer と
/// fake backend seam を提供する。
pub mod controller;
/// controller の [`controller::Effect`] を daemon-owned ポート群へ実行する本番
/// executor。effect → 実行 → `AppEvent` 還流の単方向ループを閉じる。
pub mod daemon_backend;
/// Session create/remove の pending 表示と safe landing を扱う純粋 reducer。
pub mod lifecycle;
/// daemon SessionLifecycle の effect / replay / snapshot を lifecycle reducer へ
/// 接続する adapter。
pub mod lifecycle_adapter;
/// Closeup の terminal / Agent tab と placeholder を扱う純粋 reducer。
pub mod pane;
/// daemon terminal inventory/stream と pane reducer を結合する client-side state machine。
pub mod pane_runtime;
/// Daemon-backed PR projection and browser effect ports.
pub mod pr;
/// daemon-owned generic terminal launch / attach adapter for Closeup panes.
pub mod terminal_launch;
/// Minimal VT screen grid turning raw daemon PTY output into renderable rows.
pub mod terminal_screen;
/// Pure selection and text extraction for daemon-owned terminal output.
pub mod terminal_selection;
/// Polling coordinator mirroring one daemon-owned terminal into a screen grid.
pub mod terminal_session;

/// Workspace 画面の描画に必要な、workspace identity と永続化済み state の組。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSnapshot {
    /// 開く workspace。
    pub workspace: Workspace,
    /// workspace 配下のセッション状態。
    pub state: WorkspaceState,
    /// Daemon-authoritative identity used by the Home controller.  The display
    /// [`Workspace`] remains a registry projection and is never used as an IPC
    /// target.
    pub workspace_id: WorkspaceId,
    /// Session identities in the same order as the display state projection.
    pub session_ids: Vec<SessionId>,
}

impl WorkspaceSnapshot {
    /// workspace と state を組にする。
    #[must_use]
    pub fn new(workspace: Workspace, state: WorkspaceState) -> Self {
        let session_ids = state.sessions.iter().map(|_| SessionId::new()).collect();
        Self {
            workspace,
            state,
            workspace_id: WorkspaceId::new(),
            session_ids,
        }
    }

    /// Build a snapshot whose identities came from the daemon lifecycle
    /// snapshot.  No name/path-to-ID inference is performed by the TUI.
    #[must_use]
    pub fn with_runtime_ids(
        workspace: Workspace,
        state: WorkspaceState,
        workspace_id: WorkspaceId,
        session_ids: Vec<SessionId>,
    ) -> Self {
        Self {
            workspace,
            state,
            workspace_id,
            session_ids,
        }
    }
}

/// Workspace を開く application port。
///
/// path の検証・登録・最終利用時刻の更新・state 読み込みは実 IO を持つ合成側が実装する。
/// Open 一覧と Recent はともにこの 1 つの port を経由する。
pub trait WorkspaceLoader {
    /// `path` の workspace を開き、画面描画用 snapshot を返す。
    ///
    /// # Errors
    ///
    /// workspace の解決・登録・更新・state 読み込みに失敗した場合、そのエラーを返す。
    fn open(&mut self, path: &Path) -> io::Result<WorkspaceSnapshot>;

    /// Remove entries that no longer point at directories and return the paths
    /// removed from the core-owned workspace registry. The caller has already
    /// obtained explicit user confirmation before invoking this operation.
    ///
    /// # Errors
    ///
    /// Returns an error when checking or mutating the registry fails.
    fn cleanup_missing(&mut self, workspaces: &[Workspace]) -> io::Result<Vec<PathBuf>>;

    /// Remove exactly the confirmed paths from the global workspace registry.
    /// This never removes directories or workspace-local data.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be mutated.
    fn unregister(&mut self, paths: &[PathBuf]) -> io::Result<Vec<PathBuf>>;
}

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    /// live terminal prefix（`Ctrl-O` leader）から [`LiveInputClassifier`] が解決した
    /// 予約アクション。合成ルートが classifier を保持し、leader の follow-up を
    /// このバリアントへ翻訳して渡す。非 prefix キーは従来どおり具体的な `Key` になる。
    ///
    /// [`LiveInputClassifier`]: crate::usecase::terminal_input::LiveInputClassifier
    Live(crate::usecase::terminal_input::LiveTerminalAction),
    /// Exact bytes classified as ordinary live-pane input.  This preserves
    /// paste and backend-native encodings for the focused daemon terminal.
    Passthrough(Vec<u8>),
    /// Pointer input intended for the terminal output viewport.
    Pointer(crate::usecase::terminal_input::PointerEvent),
    /// 選択を 1 つ上へ移す。
    Up,
    /// 選択を 1 つ下へ移す。
    Down,
    /// キャレットやタブを 1 つ左へ／モード選択では前の選択へ（←）。
    Left,
    /// キャレットやタブを 1 つ右へ／モード選択では次の選択へ（→）。
    Right,
    /// 選択中の項目を確定する。
    Enter,
    /// キャレット手前の 1 文字を削除する（Backspace）。
    Backspace,
    /// Overview などの入力欄で候補を補完する（Tab）。
    Tab,
    /// 一段戻る・取り消す（Esc）。最上位の画面では終了として扱う。
    Escape,
    /// 画面を終了する（Ctrl-C など）。
    Quit,
    /// Ctrl-Q ends the workspace, including its live sessions.
    CtrlQ,
    /// Ctrl-D requests an unregister confirmation only on Open Workspace.
    CtrlD,
    /// 文字キー。メニューのショートカット文字や recent の番号キーに使う。
    Char(char),
    /// 左ボタンのクリック位置（0-based terminal cell）。画面ごとの hit test は
    /// presentation が担い、座標を reducer や domain へは渡さない。
    Click { column: u16, row: u16 },
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

    /// 装飾的なアニメーションの次フレームまで待機する。実時間を直接読む代わりに
    /// 合成側へ委譲するので、presentation のテストは待機なしで検証できる。
    ///
    /// # Errors
    ///
    /// 端末ランタイムが待機に失敗した場合、そのエラーを返す。
    fn wait(&mut self, duration: Duration) -> io::Result<()>;

    /// 次のキー入力を 1 つ読む（入力があるまでブロックする）。
    ///
    /// # Errors
    ///
    /// キー入力の読み取りに失敗した場合、そのエラーを返す。
    fn read_key(&mut self) -> io::Result<Key>;

    /// Writes terminal output to the system clipboard.
    ///
    /// # Errors
    ///
    /// Returns an adapter-safe message when the clipboard is unavailable.
    #[coverage(off)]
    fn copy_text(&mut self, _text: &str) -> Result<(), String> {
        Err("clipboard is unavailable".to_owned())
    }
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
        use crate::usecase::terminal_input::LiveTerminalAction;
        // derive された Debug / Clone / PartialEq を全バリアントで実行する。
        let keys = vec![
            Key::Up,
            Key::Down,
            Key::Left,
            Key::Right,
            Key::Enter,
            Key::Tab,
            Key::Backspace,
            Key::Escape,
            Key::Quit,
            Key::CtrlD,
            Key::Char('o'),
            Key::Click { column: 3, row: 4 },
            Key::Live(LiveTerminalAction::Switch),
            Key::Passthrough(b"paste".to_vec()),
            Key::Other,
        ];
        for key in keys {
            assert_eq!(key.clone(), key);
            assert!(!format!("{key:?}").is_empty());
        }
        assert_ne!(Key::Char('a'), Key::Char('b'));
        assert_ne!(
            Key::Live(LiveTerminalAction::Switch),
            Key::Live(LiveTerminalAction::NextTab)
        );
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

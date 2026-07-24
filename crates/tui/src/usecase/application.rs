//! TUI の起動画面を選び、対応する画面 runner へ委譲する application 境界。
//!
//! CLI 面は TUI クレートへ依存できないため、CLI が要求した画面への変換は合成ルートが
//! 行う。このモジュールは変換後の [`EntryScreen`] を受け取り、画面の具体的な描画・入力
//! 処理を [`ScreenRunner`] へ委譲する。これにより画面遷移の判断を端末 IO から分離する。

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use usagi_core::domain::agent::ProviderResumeProjection;
use usagi_core::domain::id::{SessionId, WorkspaceId};
use usagi_core::domain::session_lifecycle::SessionLifecycleProjection;
use usagi_core::domain::workspace::Workspace;
use usagi_core::domain::workspace_state::WorkspaceState;

/// Daemon-authoritative Agent launch adapter for Closeup panes.
pub mod agent_launch;
/// v2 controller effect と daemon-owned Agent pane runtime を結合する host。
pub mod agent_runtime;
/// Agent tab の表示 intent を daemon inventory と照合する純粋 reducer と永続化 port。
pub mod agent_tab_intent;
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
/// Pure http(s) URL detection and validation over the ANSI-free terminal grid.
pub mod terminal_link;
/// Rendering wrapper over the shared core VT parser, projecting the screen into
/// styled/selection/cursor rows for the pane.
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
    /// ID-free provider resume state keyed by stable daemon session identity.
    pub agent_resumes: BTreeMap<SessionId, ProviderResumeProjection>,
    /// Per-session lifecycle projection keyed by stable daemon session identity,
    /// so the initial Home sidebar shows `Failed` rows and gates their actions.
    pub session_lifecycles: BTreeMap<SessionId, SessionLifecycleProjection>,
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
            agent_resumes: BTreeMap::new(),
            session_lifecycles: BTreeMap::new(),
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
            agent_resumes: BTreeMap::new(),
            session_lifecycles: BTreeMap::new(),
        }
    }

    /// Build a daemon-authoritative snapshot including the safe provider resume
    /// projection and per-session lifecycle used by the Home sidebar.
    #[must_use]
    pub fn with_runtime_projection(
        workspace: Workspace,
        state: WorkspaceState,
        workspace_id: WorkspaceId,
        session_ids: Vec<SessionId>,
        agent_resumes: BTreeMap<SessionId, ProviderResumeProjection>,
        session_lifecycles: BTreeMap<SessionId, SessionLifecycleProjection>,
    ) -> Self {
        Self {
            workspace,
            state,
            workspace_id,
            session_ids,
            agent_resumes,
            session_lifecycles,
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

    /// Create a new workspace from a validated New-project request and return
    /// the snapshot used to open it.
    ///
    /// A [`Clone`](controller::NewRequest::Clone) request fetches the repository
    /// into the destination directory; an
    /// [`Existing`](controller::NewRequest::Existing) request registers the
    /// chosen directory under its name. Either way the resulting path is then
    /// opened like any other workspace, so success lands on the same Home
    /// snapshot as [`open`](Self::open).
    ///
    /// # Errors
    ///
    /// Returns an error when cloning, registration, or opening the resulting
    /// workspace fails. Callers keep the user's form draft so the operation can
    /// be corrected and retried.
    fn create_workspace(
        &mut self,
        request: &controller::NewRequest,
    ) -> io::Result<WorkspaceSnapshot>;
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
    /// backend-native key encodings for the focused daemon terminal.
    Passthrough(Vec<u8>),
    /// A bracketed paste delivered as one block. A focused live pane wraps it in
    /// bracketed-paste markers before forwarding it to the PTY so a multi-line
    /// paste is inserted as one block instead of submitting on every embedded
    /// newline; a management text input inserts the text verbatim.
    Paste(String),
    /// An OS-native terminal copy shortcut. A focused live pane copies its
    /// selection; `fallback` reaches the PTY when there is no selection.
    TerminalCopy { fallback: Vec<u8> },
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
    /// キャレットを行頭へ（Home）。テキスト入力にフォーカスがある間はキャレット移動、
    /// navigation 文脈では `+ new session`（`Ctrl-A` と同義。#257）。
    Home,
    /// キャレットを行末へ（End）。navigation 文脈では効果を持たない。
    End,
    /// キャレット位置の 1 文字を前方削除する（Del）。
    Delete,
    /// キャレットを入力の先頭へ（`Ctrl-A`）。テキスト入力にフォーカスがある間だけ
    /// 行頭キャレットで、navigation 文脈では `+ new session` に予約されたまま（#287）。
    LineStart,
    /// キャレットを入力の末尾へ（`Ctrl-E`。`End` と等価）。
    LineEnd,
    /// 選択をキャレットから 1 文字左へ広げる（`Shift`+`←`）。
    SelectLeft,
    /// 選択をキャレットから 1 文字右へ広げる（`Shift`+`→`）。
    SelectRight,
    /// 選択を行頭まで広げる（`Shift`+`Home`）。
    SelectHome,
    /// 選択を行末まで広げる（`Shift`+`End`）。
    SelectEnd,
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
    #![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract
    use super::{EntryScreen, Key, ScreenRunner, Terminal, WorkspaceSnapshot, run};
    use std::io;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use usagi_core::domain::agent::{ProviderResumeProjection, ProviderResumeReason};
    use usagi_core::domain::id::{SessionId, WorkspaceId};
    use usagi_core::domain::workspace::Workspace;
    use usagi_core::domain::workspace_state::WorkspaceState;

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

    struct DefaultClipboardTerminal;

    impl Terminal for DefaultClipboardTerminal {
        fn size(&mut self) -> io::Result<(usize, usize)> {
            Ok((1, 1))
        }

        fn draw(&mut self, _: &[String]) -> io::Result<()> {
            Ok(())
        }

        fn wait(&mut self, _: Duration) -> io::Result<()> {
            Ok(())
        }

        fn read_key(&mut self) -> io::Result<Key> {
            Ok(Key::Quit)
        }
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
    fn default_terminal_clipboard_is_an_explicit_error() {
        assert_eq!(
            DefaultClipboardTerminal.copy_text("text"),
            Err("clipboard is unavailable".to_owned())
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
            Key::Passthrough(b"passthrough".to_vec()),
            Key::Paste("multi\nline".to_owned()),
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

    #[test]
    fn runtime_projection_constructor_preserves_daemon_resume_state() {
        let workspace_id = WorkspaceId::new();
        let session_id = SessionId::new();
        let resume = ProviderResumeProjection {
            interrupted: true,
            resumable: true,
            reason: ProviderResumeReason::ExplicitResumeAvailable,
        };
        let lifecycle = usagi_core::domain::session_lifecycle::SessionLifecycleProjection {
            lifecycle: usagi_core::domain::session_lifecycle::SessionLifecycle::Failed,
            failure_summary: Some("create failed".into()),
        };
        let snapshot = WorkspaceSnapshot::with_runtime_projection(
            Workspace::new("work", "/tmp/work"),
            WorkspaceState::default(),
            workspace_id,
            vec![session_id],
            std::collections::BTreeMap::from([(session_id, resume)]),
            std::collections::BTreeMap::from([(session_id, lifecycle.clone())]),
        );

        assert_eq!(snapshot.workspace_id, workspace_id);
        assert_eq!(snapshot.session_ids, vec![session_id]);
        assert_eq!(snapshot.agent_resumes.get(&session_id), Some(&resume));
        assert_eq!(
            snapshot.session_lifecycles.get(&session_id),
            Some(&lifecycle)
        );
    }

    #[test]
    fn runtime_identity_constructor_preserves_daemon_ids() {
        let workspace_id = WorkspaceId::new();
        let session_id = SessionId::new();
        let workspace = Workspace::new("demo", "/tmp/demo");
        let snapshot = WorkspaceSnapshot::with_runtime_ids(
            workspace.clone(),
            WorkspaceState::default(),
            workspace_id,
            vec![session_id],
        );

        assert_eq!(snapshot.workspace, workspace);
        assert_eq!(snapshot.workspace_id, workspace_id);
        assert_eq!(snapshot.session_ids, vec![session_id]);
        assert!(snapshot.agent_resumes.is_empty());
    }
}

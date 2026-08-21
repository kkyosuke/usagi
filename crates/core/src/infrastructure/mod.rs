//! 共有 infrastructure 層。TUI 面・daemon 面の両方が使う外部世界との接続
//! （IPC プロトコル型・`state.json` などの永続化・git）を実装し、domain が
//! 定義する抽象に依存する（依存方向は domain ← infrastructure）。
//! 片面しか使わない infrastructure は usagi-tui / usagi-daemon 側に置く。
//! v2 では必要になった時点で実装を追加する。
//!
//! 現在の実装は永続化ストア一式で、次のように分ける:
//! - [`paths`] — 保存先の配置。リポジトリ内メタデータ（`<repo>/.usagi`）と、
//!   既定データディレクトリ（`$USAGI_HOME` / `~/.usagi`）の解決。
//! - [`error_log`] — 日次ローテーションする実行時エラーログ。
//! - [`env_resolver`] — 設定された環境変数 binding の並列解決と、`op://` secret を読む
//!   1Password CLI（実 subprocess）。
//! - [`persistence`] — entity 非依存の永続化基盤（アトミック書き込み・ロック・
//!   markdown ＋ 派生 `index.json` の汎用ストア）。
//! - [`store`] — entity 別ストア（issue / memory / workspace レジストリ / state.json）。
//! - [`daemon`] — daemon lifecycle レコード（`daemon.json`）の store。
//! - [`workspace_state`] — workspace ごとの daemon state subtree の解決（digest・
//!   `root.json` による所属証明・legacy layout からの移行）。
//! - [`git`] — worktree ライフサイクル等の git 操作（subprocess は `GitRunner` で注入）。
//! - [`ipc`] — daemon とクライアントが Unix domain socket で交わす IPC プロトコル型と
//!   フレーミング（transport は注入）。

pub mod bounded_process;
pub mod daemon;
pub mod env_resolver;
pub mod error_log;
pub mod git;
pub mod gitignore;
pub mod ipc;
pub mod paths;
pub mod persistence;
pub mod role_catalog;
pub mod runtime_model;
pub mod store;
pub mod workspace_state;

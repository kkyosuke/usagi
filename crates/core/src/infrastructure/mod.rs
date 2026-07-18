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
//! - [`persistence`] — entity 非依存の永続化基盤（アトミック書き込み・ロック・
//!   markdown ＋ 派生 `index.json` の汎用ストア）。
//! - [`store`] — entity 別ストア（issue / memory / workspace レジストリ / state.json）。
//! - [`daemon`] — daemon lifecycle レコード（`daemon.json`）の store。
//! - [`git`] — worktree ライフサイクル等の git 操作（subprocess は `GitRunner` で注入）。
//! - [`ipc`] — daemon とクライアントが Unix domain socket で交わす IPC プロトコル型と
//!   フレーミング（transport は注入）。

pub mod daemon;
pub mod error_log;
pub mod git;
pub mod gitignore;
pub mod ipc;
pub mod paths;
pub mod persistence;
pub mod runtime_model;
pub mod store;

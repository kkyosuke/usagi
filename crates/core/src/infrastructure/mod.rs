//! 共有 infrastructure 層。TUI 面・daemon 面の両方が使う外部世界との接続
//! （IPC プロトコル型・`state.json` などの永続化・git）を実装し、domain が
//! 定義する抽象に依存する（依存方向は domain ← infrastructure）。
//! 片面しか使わない infrastructure は usagi-tui / usagi-daemon 側に置く。
//! v2 では必要になった時点で実装を追加する。
//!
//! 現在の実装は永続化ストア一式:
//! - [`repo_paths`] — リポジトリ内メタデータの配置（`<repo>/.usagi`）。
//! - [`json_file`] — アトミック書き込み・versioned JSON envelope。
//! - [`store_lock`] — ストアディレクトリの cross-process 排他ロック。
//! - [`error_log`] — 日次ローテーションする実行時エラーログ。
//! - [`markdown_store`] — frontmatter markdown ＋ 派生 `index.json` の汎用ストア。
//! - [`issue_store`] / [`memory_store`] — issue / memory の永続化。
//! - [`storage`] — 既定データディレクトリ解決と workspace レジストリ（`workspaces.json`）。

pub mod error_log;
pub mod issue_store;
pub mod json_file;
pub mod markdown_store;
pub mod memory_store;
pub mod repo_paths;
pub mod storage;
pub mod store_lock;

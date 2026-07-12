//! entity 非依存の永続化基盤。
//!
//! ここには特定のエンティティを知らない「仕組み」だけを置く。具体的なストア
//! （issue / memory / workspace）は [`super::store`] がこれらの上に構築する。
//!
//! - [`json_file`] — temp + rename のアトミック書き込みと versioned JSON envelope。
//! - [`store_lock`] — ストアディレクトリの cross-process 排他ロック（`fs2`）。
//! - [`markdown_store`] — frontmatter markdown ＋ 派生 `index.json` の汎用ストア。

pub mod json_file;
pub(crate) mod markdown_store;
pub mod store_lock;

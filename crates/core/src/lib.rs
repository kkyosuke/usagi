//! usagi-core — TUI 面と daemon 面が共有する共通クレート（common）。
//!
//! クリーンアーキテクチャの内側 2 層（domain / usecase）と、両面が共有する
//! infrastructure（IPC プロトコル型・永続化）を持つ。層内の依存方向は
//! `usecase → domain ← infrastructure` を守る。
//! このクレートは他の usagi クレート（usagi-tui / usagi-daemon）に依存しない。
//! 実 IO（標準入出力・サブプロセス・端末）は合成ルート（ルートパッケージの
//! `main.rs`）で束ね、各層は依存注入によりユニットテスト可能に保つ。

pub mod domain;
pub mod infrastructure;
pub mod usecase;

#[cfg(test)]
pub(crate) mod test_support;

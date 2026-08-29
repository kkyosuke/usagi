#![feature(coverage_attribute)]

//! usagi-core — TUI 面と daemon 面が共有する共通クレート（common）。
//!
//! domain と、共有 application logic（usecase）、両面が共有する technical boundary
//! （infrastructure: IPC プロトコル型・永続化・Git）を持つ。domain は外側へ依存しない。
//! usecase と infrastructure は、transaction contract とその共有実装を同じ common crate に
//! 閉じるため相互の型を参照できる。この実際の依存行列は document/02-architecture.md が正本。
//! このクレートは他の usagi クレート（usagi-tui / usagi-daemon）に依存しない。
//! 実 IO（標準入出力・サブプロセス・端末）は合成ルート（ルートパッケージの
//! `main.rs`）で束ね、各層は依存注入によりユニットテスト可能に保つ。

pub mod domain;
pub mod infrastructure;
pub mod usecase;

#[cfg(test)]
pub(crate) mod test_support;

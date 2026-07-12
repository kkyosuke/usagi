//! usagi v2 のライブラリクレート。
//!
//! クリーンアーキテクチャの 4 層で構成し、依存方向は
//! `presentation → usecase → domain ← infrastructure` を守る。
//! 実 IO（標準入出力・サブプロセス・端末）は合成ルート（`main.rs`）で束ね、
//! 各層は依存注入によりユニットテスト可能に保つ。

pub mod domain;
pub mod infrastructure;
pub mod presentation;
pub mod usecase;

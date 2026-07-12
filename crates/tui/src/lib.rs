#![feature(coverage_attribute)]

//! usagi-tui — daemon が所有する端末に attach するクライアント側
//! （画面描画・キー入力・attach プロトコルのクライアント）をここに実装する。
//! usagi-core にのみ依存し、usagi-daemon には依存しない（daemon との通信は
//! usagi-core の IPC プロトコル型を介して行う）。
//! 実 IO は行わず、入出力は呼び出し側（合成ルート）から注入する。
//!
//! クレート内はクリーンアーキテクチャの層で分ける（依存方向
//! `presentation → usecase → domain ← infrastructure`。domain は共有のため
//! usagi-core が持つ）。描画・入力・画面遷移・attach クライアントは TUI 面ローカルで、
//! core には移さない（core が持つのは共有 data・IPC プロトコル型・永続化のみ）。
//! 各層の責務は document/02-architecture.md が正本。

pub mod infrastructure;
pub mod presentation;
pub mod usecase;

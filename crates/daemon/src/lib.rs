//! usagi-daemon — 常駐プロセス（`usagi daemon`）のサーバ面クレート。
//!
//! agent / シェルの PTY 所有・セッション監視・委譲 queue の消化
//! （document/proposals/02-daemon.md）をここに実装する。
//! usagi-core にのみ依存し、usagi-tui には依存しない（TUI との通信は
//! usagi-core の IPC プロトコル型を介して行う）。
//!
//! クレート内はクリーンアーキテクチャの層で分ける。各面が共有する domain /
//! usecase / IPC プロトコル型・永続化は usagi-core が持ち、このクレートには
//! **daemon 専用**のロジックだけを置く（監視ティックの駆動・autostart queue の
//! 消化・通知調停は `usecase/`、PTY 所有・IPC socket サーバ・daemon lifecycle の
//! 永続化は `infrastructure/`、IPC リクエストの dispatch は `presentation/`）。
//! domain は usagi-core を再利用するため、このクレートには置かない。
//! 実 IO は行わず、入出力は呼び出し側（合成ルート）から注入する。

pub mod infrastructure;
pub mod presentation;
pub mod usecase;

#[cfg(test)]
mod test_support;

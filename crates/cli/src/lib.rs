#![feature(coverage_attribute)]

//! usagi-cli — 入口面クレート。常駐しない 2 つの入口 — 人間向け CLI
//! サブコマンド（`cli`）とエージェント向け MCP サーバ（`mcp`）—をここに実装する
//! （document/proposals/01-entry-surfaces.md）。
//! usagi-core にのみ依存し、usagi-tui / usagi-daemon には依存しない
//! （daemon との連携は usagi-core の IPC プロトコル型を介した実行時通信のみ）。
//! 実 IO は行わず、入出力は呼び出し側（合成ルート）から注入する。

pub mod cli;
pub mod mcp;

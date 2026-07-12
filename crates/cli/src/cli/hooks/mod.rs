//! エージェント統合フックの内部コマンド置き場。usagi はエージェント起動時に、Claude の
//! フックへ次を配線する — `PreToolUse` へ `usagi guard-workspace`、Stop へ
//! `usagi agent-phase <phase>`。人間向けコマンド（[`crate::cli::commands`]）とは呼び手も
//! 目的も違う（人手でもエージェントの推論でもなく、エージェントのハーネスが自動実行する）
//! ため、ここに分離する。
//!
//! MCP tool と違い、Claude のフックはシェルコマンドしか呼べないので、この統合は CLI
//! コマンドとして持つしかない。`--help` には出さない（`hide = true`）が、CLI コマンド
//! ツリーの一部として同じ `Run` dispatch に載る。
//!
//! 現状はどちらも枠だけで、実挙動（guard の enforcing・phase の daemon 報告）は
//! daemon / orchestration の実装時に入れる。フックは終了コードだけを見るため、いまは
//! 黙って正常終了する。

pub mod agent_phase;
pub mod guard_workspace;

pub use agent_phase::AgentPhase;
pub use guard_workspace::GuardWorkspace;

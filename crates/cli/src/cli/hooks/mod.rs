//! エージェント統合フックの内部コマンド置き場。Codex の `SessionStart` structured
//! capture、Claude の `PreToolUse` / Stop など、agent harness が自動実行する入口を
//! 人間向けコマンド（[`crate::cli::commands`]）から分離する。
//!
//! MCP tool と違い、Claude のフックはシェルコマンドしか呼べないので、この統合は CLI
//! コマンドとして持つしかない。`--help` には出さない（`hide = true`）が、CLI コマンド
//! ツリーの一部として同じ `Run` dispatch に載る。
//!
//! Codex capture は documented stdin JSON を private daemon request に変換する。Claude の
//! `guard-workspace` は `PreToolUse` payload を検査し、worktree を出るツール呼び出しを deny する
//! （判定は [`usagi_core::usecase::workspace_guard`]）。phase 報告はまだ枠だけで終了コード 0 を返す。

pub mod agent_phase;
pub mod codex_session_capture;
pub mod guard_workspace;

pub use agent_phase::AgentPhase;
pub use codex_session_capture::CodexSessionCapture;
pub use guard_workspace::GuardWorkspace;

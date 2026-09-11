//! エージェント統合フックの内部コマンド置き場。Claude / Codex の `SessionStart`
//! structured capture や `PreToolUse` / Stop など、agent harness が自動実行する入口を
//! 人間向けコマンド（[`crate::cli::commands`]）から分離する。
//!
//! provider の command hook から呼び出せるよう、この統合は hidden CLI コマンドとして
//! 持つ。`--help` には出さない（`hide = true`）が、CLI コマンドツリーの一部として同じ
//! `Run` dispatch に載る。
//!
//! lifecycle phase と structured starting event（Claude / Codex の `SessionStart`、
//! Antigravity の `PreInvocation`）の current provider ID は documented stdin JSON から
//! private daemon request へ変換する。Claude の `guard-workspace` は `PreToolUse` payload を
//! 検査し、worktree を出るツール呼び出しを deny する（判定は
//! [`usagi_core::usecase::workspace_guard`]）。

pub mod agent_phase;
pub mod claude_sandbox;
pub mod codex_session_capture;
pub mod guard_workspace;

pub use agent_phase::AgentPhase;
pub use claude_sandbox::ClaudeSandbox;
pub use codex_session_capture::CodexSessionCapture;
pub use guard_workspace::GuardWorkspace;

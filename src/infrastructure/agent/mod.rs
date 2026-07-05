//! Agent adapters: one per CLI, each converting usagi's [`Agent`] interface into
//! that CLI's own invocation.
//!
//! [`agent_for`] is the single place that maps the configured [`AgentCli`] to its
//! adapter, so adding a new agent is one adapter module plus one arm here. Each
//! adapter renders its own launch command from the domain's
//! [`AgentWiring`](crate::domain::agent::AgentWiring) policy and is tested in its
//! own module.

mod antigravity;
mod claude;
mod codex;
mod gemini;
mod util;

use std::sync::Arc;

pub use antigravity::AntigravityAgent;
pub use claude::ClaudeAgent;
pub use codex::CodexAgent;
pub use gemini::GeminiAgent;

use crate::domain::agent::Agent;
use crate::domain::settings::AgentCli;

/// System-prompt addendum injected into agents launched from a usagi session.
///
/// Every agent usagi starts already lives inside the session's dedicated
/// worktree, so the usual "create a worktree first" workflow step is redundant
/// here. We tell the agent up front to skip it and work in place. Kept free of
/// single quotes so it survives a single-quoted shell argument verbatim. Shared
/// by every adapter: those with a system-prompt flag inject it out of band
/// (Claude via `--append-system-prompt`, Codex via `developer_instructions`),
/// and those without one (Gemini, Antigravity) lead their opening prompt with it
/// via [`session_opening_prompt`].
pub(super) const SESSION_WORKTREE_PROMPT: &str = "あなたは usagi が管理するセッション専用の worktree 内で起動されています。このディレクトリは既に独立した作業環境のため、新たに git worktree を作成する必要はありません。ここで直接作業を進めてください。なお、この worktree は親のメインリポジトリの内側に置かれていますが、作業はこのディレクトリ配下だけで完結させ、親ディレクトリ（メインリポジトリ本体）のファイルは読み書きせず、そこへ cd もしないでください。";

/// System-prompt addendum added when a local LLM MCP server is wired in.
///
/// It nudges the cloud agent to offload light, low-stakes work (summaries,
/// naming, boilerplate, simple transforms) to the `local_llm_ask` tool so the
/// cloud model's tokens are spent on the work that actually needs it. Kept free
/// of single quotes so it survives a single-quoted shell argument verbatim.
const LOCAL_LLM_PROMPT: &str = "トークン節約のため、要約・命名・定型文の生成・単純な変換といった軽量で重要度の低いタスクは、MCP ツール local_llm_ask（ローカル LLM）に委譲してください。判断が必要な作業や重要な実装はあなた自身が行ってください。";

/// The session system-prompt text handed to a launched agent: the worktree note,
/// plus the local-LLM delegation nudge when a local model is wired in. The
/// adapters render it into their CLI's own flag (Claude `--append-system-prompt`,
/// Codex `developer_instructions`).
pub(super) fn session_system_prompt(local_llm_model: Option<&str>) -> String {
    match local_llm_model {
        Some(_) => format!("{SESSION_WORKTREE_PROMPT}{LOCAL_LLM_PROMPT}"),
        None => SESSION_WORKTREE_PROMPT.to_string(),
    }
}

/// The opening prompt for an agent that has no system-prompt flag (Gemini,
/// Antigravity). These agents can't be told out of band that they already live in
/// a usagi worktree, so the worktree note leads their opening prompt — delivered as
/// the first conversational turn — with the user's queued prompt (if any) following
/// after a blank line. The local-LLM delegation nudge is deliberately omitted: with
/// no MCP wiring these agents have no `local_llm_ask` tool to delegate to. The
/// result is escaped for the shell by the caller (the note itself carries no single
/// quotes; the queued prompt may, and is escaped along with it).
pub(super) fn session_opening_prompt(initial_prompt: Option<&str>) -> String {
    match initial_prompt {
        Some(prompt) => format!("{SESSION_WORKTREE_PROMPT}\n\n{prompt}"),
        None => SESSION_WORKTREE_PROMPT.to_string(),
    }
}

/// The agent adapter for the configured CLI, shared via `Arc`.
pub fn agent_for(cli: AgentCli) -> Arc<dyn Agent> {
    match cli {
        AgentCli::Claude => Arc::new(ClaudeAgent::new()),
        AgentCli::Codex => Arc::new(CodexAgent::new()),
        AgentCli::CodexFugu => Arc::new(CodexAgent::fugu()),
        AgentCli::Gemini => Arc::new(GeminiAgent::new()),
        AgentCli::Antigravity => Arc::new(AntigravityAgent::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::settings::Settings;

    #[test]
    fn agent_for_claude_renders_the_launch_command() {
        // The Claude adapter delegates launch rendering to the domain builder
        // (MCP servers + system prompt + lifecycle hooks).
        let agent = agent_for(AgentCli::Claude);
        assert_eq!(agent.program(), "claude");
        let launch = agent.launch_command(&Settings::default().agent_wiring("usagi"), false, None);
        assert!(launch.starts_with("claude --mcp-config '{\"mcpServers\":"));
        assert!(launch.contains("--append-system-prompt"));
        assert!(launch.contains("--settings '{\"hooks\":"));
        // Resuming routes through to Claude's `--continue` flag.
        let resumed = agent.launch_command(&Settings::default().agent_wiring("usagi"), true, None);
        assert!(resumed.starts_with("claude --continue --mcp-config '"));
    }

    #[test]
    fn agent_for_codex_wires_in_the_usagi_mcp_server_and_hooks() {
        // The Codex adapter wires the unified usagi MCP server in via Codex's `-c`
        // config overrides and reports its phase through Codex lifecycle hooks.
        let agent = agent_for(AgentCli::Codex);
        assert_eq!(agent.program(), "codex");
        let launch = agent.launch_command(&Settings::default().agent_wiring("usagi"), false, None);
        assert!(launch.starts_with("codex --dangerously-bypass-hook-trust "));
        assert!(launch.contains("-c 'mcp_servers.usagi.command=usagi'"));
        assert!(launch.contains("usagi agent-phase ready"));
        assert!(launch.contains("usagi agent-phase ended"));
    }

    #[test]
    fn agent_for_codex_fugu_wires_like_codex_under_the_codex_fugu_program() {
        // codex-fugu reuses the Codex adapter, so it gets the same MCP wiring and
        // lifecycle hooks — only the launched program name differs.
        let agent = agent_for(AgentCli::CodexFugu);
        assert_eq!(agent.program(), "codex-fugu");
        let launch = agent.launch_command(&Settings::default().agent_wiring("usagi"), false, None);
        assert!(launch.starts_with("codex-fugu --dangerously-bypass-hook-trust "));
        assert!(launch.contains("-c 'mcp_servers.usagi.command=usagi'"));
        assert!(launch.contains("usagi agent-phase ready"));
        assert!(launch.contains("usagi agent-phase ended"));
    }

    #[test]
    fn agent_for_gemini_leads_with_the_worktree_note() {
        // Gemini has no inline-injection flag, so the MCP/hooks wiring is not
        // rendered; the session worktree note still leads the opening prompt.
        let agent = agent_for(AgentCli::Gemini);
        assert_eq!(agent.program(), "gemini");
        let launch = agent.launch_command(&Settings::default().agent_wiring("usagi"), false, None);
        assert!(launch.starts_with("gemini -i='"));
        assert!(launch.contains(SESSION_WORKTREE_PROMPT));
        assert!(!launch.contains("mcp"));
    }

    #[test]
    fn agent_for_antigravity_leads_with_the_worktree_note() {
        // Antigravity (`agy`) has no inline-injection flag, so like Gemini the
        // MCP/hooks wiring is not rendered; the session worktree note still leads the
        // opening prompt.
        let agent = agent_for(AgentCli::Antigravity);
        assert_eq!(agent.program(), "agy");
        let launch = agent.launch_command(&Settings::default().agent_wiring("usagi"), false, None);
        assert!(launch.starts_with("agy --dangerously-skip-permission -i='"));
        assert!(launch.contains(SESSION_WORKTREE_PROMPT));
        assert!(!launch.contains("mcp"));
    }
}

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
/// The system-prompt text for a coordinator running at the workspace root.
pub(super) const ROOT_PROMPT: &str = "<context>\nあなたは usagi が管理するワークスペースの root ディレクトリ（統括環境）で起動されています。\n</context>\n<instructions>\n受け取った指示や issue をもとに、どのようなタスクを各セッションに実行させるべきかを判別してください。\n</instructions>";

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
pub(super) const SESSION_WORKTREE_PROMPT: &str = "<context>\nあなたは usagi が管理するセッション専用の worktree 内で起動されています。このディレクトリは既に独立した作業環境のため、新たに git worktree を作成する必要はありません。\n</context>\n<constraints>\n- 作業はこのディレクトリ配下だけで完結させてください。\n- 親ディレクトリ（メインリポジトリ本体）のファイルは読み書きしないでください。\n- 親ディレクトリへ cd しないでください。\n</constraints>\n<instructions>\n受けた指示を実行して、何かしらの結果（設計やPRなど）みれる形で提供してください。\n</instructions>";

/// The suffix prompt added to Gemini/Antigravity launches to prevent immediate action
/// on this instruction alone.
pub(super) const GEMINI_AGY_ADDENDUM: &str = "<system_note>\nこの指示だけで実行はさせず、次の指示を待ってから作業に取り掛かってください。\n</system_note>";

/// System-prompt addendum added when a local LLM MCP server is wired in.
///
/// It nudges the cloud agent to offload light, low-stakes work (summaries,
/// naming, boilerplate, simple transforms) to the `local_llm_ask` tool so the
/// cloud model's tokens are spent on the work that actually needs it. Kept free
/// of single quotes so it survives a single-quoted shell argument verbatim.
const LOCAL_LLM_PROMPT: &str = "<delegation_instructions>\nトークン節約のため、要約・命名・定型文の生成・単純な変換といった軽量で重要度の低いタスクは、MCP ツール local_llm_ask（ローカル LLM）に委譲してください。判断が必要な作業や重要な実装はあなた自身が行ってください。\n</delegation_instructions>";

/// Resolve the base agent prompt depending on whether it is root and whether it is a
/// gemini/agy agent.
pub(super) fn base_agent_prompt(is_root: bool, is_gemini_agy: bool) -> String {
    let base = if is_root {
        ROOT_PROMPT.to_string()
    } else {
        SESSION_WORKTREE_PROMPT.to_string()
    };
    if is_gemini_agy {
        format!("{base}\n{GEMINI_AGY_ADDENDUM}")
    } else {
        base
    }
}

/// The session system-prompt text handed to a launched agent: the base prompt,
/// plus the local-LLM delegation nudge when a local model is wired in. The
/// adapters render it into their CLI's own flag (Claude `--append-system-prompt`,
/// Codex `developer_instructions`).
pub(super) fn session_system_prompt(
    is_root: bool,
    is_gemini_agy: bool,
    local_llm_model: Option<&str>,
) -> String {
    let base = base_agent_prompt(is_root, is_gemini_agy);
    match local_llm_model {
        Some(_) => format!("{base}\n{LOCAL_LLM_PROMPT}"),
        None => base,
    }
}

/// The opening prompt for an agent that has no system-prompt flag (Gemini,
/// Antigravity). These agents can't be told out of band that they already live in
/// a usagi worktree, so the base prompt leads their opening prompt — delivered as
/// the first conversational turn — with the user's queued prompt (if any) following
/// after a blank line. The local-LLM delegation nudge is deliberately omitted: with
/// no MCP wiring these agents have no `local_llm_ask` tool to delegate to. The
/// result is escaped for the shell by the caller (the note itself carries no single
/// quotes; the queued prompt may, and is escaped along with it).
pub(super) fn session_opening_prompt(
    is_root: bool,
    is_gemini_agy: bool,
    initial_prompt: Option<&str>,
) -> String {
    let base = base_agent_prompt(is_root, is_gemini_agy);
    match initial_prompt {
        Some(prompt) => format!("{base}\n\n{prompt}"),
        None => base,
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

        // 1. Session worktree case (is_root = false)
        let mut wiring = Settings::default().agent_wiring("usagi");
        wiring.is_root = false;
        let launch = agent.launch_command(&wiring, false, None);
        assert!(launch.starts_with("gemini -i='"));
        assert!(launch.contains(SESSION_WORKTREE_PROMPT));
        assert!(launch.contains(GEMINI_AGY_ADDENDUM));
        assert!(!launch.contains("mcp"));

        // 2. Root coordinator case (is_root = true)
        let mut wiring = Settings::default().agent_wiring("usagi");
        wiring.is_root = true;
        let launch = agent.launch_command(&wiring, false, None);
        assert!(launch.starts_with("gemini -i='"));
        assert!(launch.contains(ROOT_PROMPT));
        assert!(launch.contains(GEMINI_AGY_ADDENDUM));
    }

    #[test]
    fn agent_for_antigravity_leads_with_the_worktree_note() {
        // Antigravity (`agy`) has no inline-injection flag, so like Gemini the
        // MCP/hooks wiring is not rendered; the session worktree note still leads the
        // opening prompt.
        let agent = agent_for(AgentCli::Antigravity);
        assert_eq!(agent.program(), "agy");

        // 1. Session worktree case (is_root = false)
        let mut wiring = Settings::default().agent_wiring("usagi");
        wiring.is_root = false;
        let launch = agent.launch_command(&wiring, false, None);
        assert!(launch.starts_with("agy -i='"));
        assert!(launch.contains(SESSION_WORKTREE_PROMPT));
        assert!(launch.contains(GEMINI_AGY_ADDENDUM));
        assert!(!launch.contains("mcp"));

        // 2. Root coordinator case (is_root = true)
        let mut wiring = Settings::default().agent_wiring("usagi");
        wiring.is_root = true;
        let launch = agent.launch_command(&wiring, false, None);
        assert!(launch.starts_with("agy -i='"));
        assert!(launch.contains(ROOT_PROMPT));
        assert!(launch.contains(GEMINI_AGY_ADDENDUM));
    }
}

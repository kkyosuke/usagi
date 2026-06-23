//! Agent adapters: one per CLI, each converting usagi's [`Agent`] interface into
//! that CLI's own invocation.
//!
//! [`agent_for`] is the single place that maps the configured [`AgentCli`] to its
//! adapter, so adding a new agent is one adapter module plus one arm here. Each
//! adapter renders its own launch command from the domain's
//! [`AgentWiring`](crate::domain::agent::AgentWiring) policy and is tested in its
//! own module.

mod claude;
mod codex;
mod gemini;

use std::sync::Arc;

pub use claude::ClaudeAgent;
pub use codex::CodexAgent;
pub use gemini::GeminiAgent;

use crate::domain::agent::Agent;
use crate::domain::settings::AgentCli;

/// The agent adapter for the configured CLI, shared via `Arc`.
pub fn agent_for(cli: AgentCli) -> Arc<dyn Agent> {
    match cli {
        AgentCli::Claude => Arc::new(ClaudeAgent::new()),
        AgentCli::Codex => Arc::new(CodexAgent::new()),
        AgentCli::Gemini => Arc::new(GeminiAgent::new()),
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
    fn agent_for_gemini_launches_plain() {
        let agent = agent_for(AgentCli::Gemini);
        assert_eq!(agent.program(), "gemini");
        assert_eq!(
            agent.launch_command(&Settings::default().agent_wiring("usagi"), false, None),
            "gemini"
        );
    }
}

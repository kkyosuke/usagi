//! Agent adapters: one per CLI, each converting usagi's [`Agent`] interface into
//! that CLI's own invocation.
//!
//! [`agent_for`] is the single place that maps the configured [`AgentCli`] to its
//! adapter, so adding a new agent is one adapter module plus one arm here. The
//! pure piece — the Claude launch-command rendering — is tested in the domain
//! settings builder it delegates to.

mod claude;
mod gemini;

use std::sync::Arc;

pub use claude::ClaudeAgent;
pub use gemini::GeminiAgent;

use crate::domain::agent::Agent;
use crate::domain::settings::AgentCli;

/// The agent adapter for the configured CLI, shared via `Arc`.
pub fn agent_for(cli: AgentCli) -> Arc<dyn Agent> {
    match cli {
        AgentCli::Claude => Arc::new(ClaudeAgent::new()),
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
        let launch = agent.launch_command(&Settings::default().agent_wiring("usagi"), false);
        assert!(launch.starts_with("claude --mcp-config '{\"mcpServers\":"));
        assert!(launch.contains("--append-system-prompt"));
        assert!(launch.contains("--settings '{\"hooks\":"));
        // Resuming routes through to Claude's `--continue` flag.
        let resumed = agent.launch_command(&Settings::default().agent_wiring("usagi"), true);
        assert!(resumed.starts_with("claude --continue --mcp-config '"));
    }

    #[test]
    fn agent_for_gemini_launches_plain() {
        let agent = agent_for(AgentCli::Gemini);
        assert_eq!(agent.program(), "gemini");
        assert_eq!(
            agent.launch_command(&Settings::default().agent_wiring("usagi"), false),
            "gemini"
        );
    }
}

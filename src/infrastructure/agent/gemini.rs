//! Gemini CLI adapter.
//!
//! Gemini takes its MCP servers from `settings.json` rather than inline flags, so
//! it launches as the bare command for now, and it exposes no transcript usagi
//! can read for context-window usage — so [`Agent::usage`] reports nothing. The
//! adapter exists so the sidebar gauge and launch path work for Gemini sessions
//! today and start reporting the moment a source is wired in here, with no change
//! to the call sites.

use std::path::Path;

use crate::domain::agent::{Agent, AgentWiring};
use crate::domain::agent_usage::AgentUsage;
use crate::domain::settings::AgentCli;

/// The Gemini CLI adapter.
#[derive(Default)]
pub struct GeminiAgent;

impl GeminiAgent {
    /// A Gemini adapter.
    pub fn new() -> Self {
        Self
    }
}

impl Agent for GeminiAgent {
    fn program(&self) -> &'static str {
        "gemini"
    }

    fn launch_command(&self, wiring: &AgentWiring) -> String {
        // Gemini has no inline MCP flag — its servers come from settings.json — so
        // it launches plain (the domain builder returns the bare command).
        AgentCli::Gemini.launch_command(wiring.local_llm_model.as_deref(), &wiring.usagi_bin)
    }

    fn usage(&self, _worktree: &Path) -> Option<AgentUsage> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::settings::Settings;
    use std::path::PathBuf;

    #[test]
    fn gemini_launches_plain_and_reports_no_usage() {
        let agent = GeminiAgent::new();
        assert_eq!(agent.program(), "gemini");
        // The wiring is ignored — plain `gemini` whether or not the local LLM is on.
        assert_eq!(
            agent.launch_command(&Settings::default().agent_wiring("usagi")),
            "gemini"
        );
        let mut settings = Settings::default();
        settings.local_llm.enabled = true;
        assert_eq!(
            agent.launch_command(&settings.agent_wiring("usagi")),
            "gemini"
        );
        assert_eq!(agent.usage(&PathBuf::from("/anywhere")), None);
    }
}

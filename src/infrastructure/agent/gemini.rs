//! Gemini CLI adapter.
//!
//! Gemini takes its MCP servers from `settings.json` rather than inline flags, so
//! it launches as the bare command for now. The adapter exists so the launch path
//! works for Gemini sessions today, with no change to the call sites.

use std::path::Path;

use crate::domain::agent::{Agent, AgentWiring};

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

    fn launch_command(
        &self,
        _wiring: &AgentWiring,
        _resume: bool,
        _initial_prompt: Option<&str>,
    ) -> String {
        // Gemini has no inline MCP flag — its servers come from settings.json — so
        // it launches as the bare command, ignoring the wiring. It also has no
        // resume flag or usagi-driven opening-prompt path, so `resume` and
        // `initial_prompt` are ignored too.
        "gemini".to_string()
    }

    fn has_resumable_session(&self, _dir: &Path) -> bool {
        // Gemini has no resume notion usagi drives, so it always launches fresh.
        false
    }

    fn forget_session(&self, _dir: &Path) {
        // usagi keeps no Gemini conversation store, so there is nothing to clear.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::settings::Settings;

    #[test]
    fn gemini_launches_plain() {
        let agent = GeminiAgent::new();
        assert_eq!(agent.program(), "gemini");
        // The wiring is ignored — plain `gemini` whether or not the local LLM is on.
        assert_eq!(
            agent.launch_command(&Settings::default().agent_wiring("usagi"), false, None),
            "gemini"
        );
        let mut settings = Settings::default();
        settings.local_llm.enabled = true;
        assert_eq!(
            agent.launch_command(&settings.agent_wiring("usagi"), false, None),
            "gemini"
        );
        // Gemini has no resume flag, so requesting one still launches plain — and
        // it never reports a resumable session.
        assert_eq!(
            agent.launch_command(&Settings::default().agent_wiring("usagi"), true, None),
            "gemini"
        );
        // An opening prompt is ignored too: Gemini still launches plain.
        assert_eq!(
            agent.launch_command(
                &Settings::default().agent_wiring("usagi"),
                false,
                Some("hi")
            ),
            "gemini"
        );
        assert!(!agent.has_resumable_session(std::path::Path::new("/any/worktree")));
        // Forgetting a session is a no-op for Gemini (no conversation store).
        agent.forget_session(std::path::Path::new("/any/worktree"));
    }
}

//! Claude Code adapter.
//!
//! Builds Claude's launch command, delegating to the pure
//! [`AgentCli::launch_command`], which wires in usagi's MCP servers, system
//! prompt, and lifecycle hooks. That rendering is pure and tested in the domain
//! settings builder.

use crate::domain::agent::{Agent, AgentWiring};
use crate::domain::settings::AgentCli;

/// The Claude Code adapter.
#[derive(Default)]
pub struct ClaudeAgent;

impl ClaudeAgent {
    /// A Claude adapter.
    pub fn new() -> Self {
        Self
    }
}

impl Agent for ClaudeAgent {
    fn program(&self) -> &'static str {
        "claude"
    }

    fn launch_command(&self, wiring: &AgentWiring) -> String {
        AgentCli::Claude.launch_command(wiring.local_llm_model.as_deref(), &wiring.usagi_bin)
    }
}

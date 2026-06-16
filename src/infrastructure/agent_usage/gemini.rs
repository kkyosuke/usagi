//! Gemini CLI usage reader.
//!
//! Gemini does not expose a transcript usagi can read for context-window usage
//! yet, so this reports nothing. It implements [`UsageReader`] so the sidebar
//! gauge keeps working for Gemini sessions (showing no usage) and starts
//! reporting the moment a source is wired in here, with no change to the call
//! sites.

use std::path::Path;

use crate::domain::agent_usage::{AgentUsage, UsageReader};

/// A usage reader for the Gemini CLI. Reports no usage until Gemini exposes a
/// readable transcript.
#[derive(Default)]
pub struct GeminiUsageReader;

impl GeminiUsageReader {
    /// A Gemini usage reader.
    pub fn new() -> Self {
        Self
    }
}

impl UsageReader for GeminiUsageReader {
    fn read(&self, _worktree: &Path) -> Option<AgentUsage> {
        None
    }
}

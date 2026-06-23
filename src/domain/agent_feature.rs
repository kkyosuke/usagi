//! The agent-feature support matrix: which usagi integrations each
//! [`AgentCli`] receives.
//!
//! Different agent CLIs expose different hooks for usagi to wire into (inline
//! config flags, hook systems, resume flags …), so usagi can offer more to some
//! than others. This module is the **single source of truth** for that matrix —
//! the [`AgentFeature`] rows, and [`support`] declaring each CLI's support for
//! each — rendered by the `usagi feature` command
//! ([`crate::presentation::cli::feature`]). [`support`] matches exhaustively over
//! every CLI, so adding an [`AgentCli`] variant is a compile error until its
//! support is declared here.

use crate::domain::settings::AgentCli;

/// One usagi integration feature — a row of the support matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentFeature {
    /// The unified `usagi` MCP server (issue / memory / session tools).
    Mcp,
    /// The `usagi-llm` MCP server for delegating light work to a local LLM.
    LocalLlmMcp,
    /// Lifecycle-hook phase reporting (ready / running / waiting / ended).
    PhaseReporting,
    /// Starting the session already working on a queued `session_prompt`.
    InitialPrompt,
    /// Injecting usagi's session system prompt (worktree note + delegation nudge).
    SystemPrompt,
    /// Resuming the worktree's previous conversation on relaunch.
    Resume,
    /// Discarding the worktree's conversation history on `session remove`.
    ForgetHistory,
}

impl AgentFeature {
    /// The features in the order the matrix lists them, top to bottom.
    pub const ALL: [AgentFeature; 7] = [
        AgentFeature::Mcp,
        AgentFeature::LocalLlmMcp,
        AgentFeature::PhaseReporting,
        AgentFeature::InitialPrompt,
        AgentFeature::SystemPrompt,
        AgentFeature::Resume,
        AgentFeature::ForgetHistory,
    ];

    /// The human-readable row label.
    pub fn label(self) -> &'static str {
        match self {
            AgentFeature::Mcp => "MCP（issue / memory / session）",
            AgentFeature::LocalLlmMcp => "MCP（ローカル LLM 委譲）",
            AgentFeature::PhaseReporting => "状態報告（フック）",
            AgentFeature::InitialPrompt => "初期プロンプト",
            AgentFeature::SystemPrompt => "system prompt 注入",
            AgentFeature::Resume => "会話の再開",
            AgentFeature::ForgetHistory => "会話履歴の破棄",
        }
    }
}

/// Whether an [`AgentCli`] receives a feature through usagi.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// usagi wires the feature in for this CLI.
    Yes,
    /// The CLI cannot offer it through usagi (e.g. no inline-injection flag, no
    /// hook mechanism).
    No,
}

impl Support {
    /// A compact glyph for terminal tables.
    pub fn glyph(self) -> &'static str {
        match self {
            Support::Yes => "✅",
            Support::No => "❌",
        }
    }
}

/// How `cli` supports `feature` through usagi — the support matrix itself.
///
/// Claude, Codex, and codex-fugu receive every integration (Claude via its native
/// flags; Codex and the codex-fugu variant via `-c` config overrides and Codex's
/// hook system). Gemini has no
/// inline-injection flag and no usagi-drivable hook system, so its MCP servers,
/// phase-reporting hooks, and system prompt cannot be wired; only the plain flags
/// it does expose — an opening prompt (`-i`), resume (`-r latest`), and the chat
/// store usagi can clear — are supported.
pub fn support(cli: AgentCli, feature: AgentFeature) -> Support {
    match cli {
        AgentCli::Claude | AgentCli::Codex | AgentCli::CodexFugu => Support::Yes,
        AgentCli::Gemini => match feature {
            AgentFeature::InitialPrompt | AgentFeature::Resume | AgentFeature::ForgetHistory => {
                Support::Yes
            }
            AgentFeature::Mcp
            | AgentFeature::LocalLlmMcp
            | AgentFeature::PhaseReporting
            | AgentFeature::SystemPrompt => Support::No,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_feature_has_a_nonempty_label() {
        // ALL lists each feature once and every label is rendered.
        assert_eq!(AgentFeature::ALL.len(), 7);
        for feature in AgentFeature::ALL {
            assert!(!feature.label().is_empty());
        }
    }

    #[test]
    fn glyphs_distinguish_support() {
        assert_eq!(Support::Yes.glyph(), "✅");
        assert_eq!(Support::No.glyph(), "❌");
        assert_ne!(Support::Yes.glyph(), Support::No.glyph());
    }

    #[test]
    fn claude_and_codex_support_every_feature() {
        for feature in AgentFeature::ALL {
            assert_eq!(support(AgentCli::Claude, feature), Support::Yes);
            assert_eq!(support(AgentCli::Codex, feature), Support::Yes);
            // codex-fugu reuses the Codex adapter, so it supports the same set.
            assert_eq!(support(AgentCli::CodexFugu, feature), Support::Yes);
        }
    }

    #[test]
    fn gemini_supports_only_the_plain_flag_features() {
        // What Gemini's plain CLI flags expose, usagi wires.
        assert_eq!(
            support(AgentCli::Gemini, AgentFeature::InitialPrompt),
            Support::Yes
        );
        assert_eq!(
            support(AgentCli::Gemini, AgentFeature::Resume),
            Support::Yes
        );
        assert_eq!(
            support(AgentCli::Gemini, AgentFeature::ForgetHistory),
            Support::Yes
        );
        // What needs inline injection / a hook system, it cannot.
        assert_eq!(support(AgentCli::Gemini, AgentFeature::Mcp), Support::No);
        assert_eq!(
            support(AgentCli::Gemini, AgentFeature::LocalLlmMcp),
            Support::No
        );
        assert_eq!(
            support(AgentCli::Gemini, AgentFeature::PhaseReporting),
            Support::No
        );
        assert_eq!(
            support(AgentCli::Gemini, AgentFeature::SystemPrompt),
            Support::No
        );
    }
}

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
    /// Lifecycle phase reporting (ready / running / waiting / ended / exited).
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
    /// A compact terminal glyph for support status.
    pub fn glyph(self) -> &'static str {
        match self {
            Support::Yes => "✓",
            Support::No => "—",
        }
    }
}

/// The set of usagi integrations an [`AgentCli`] receives, as one descriptor.
///
/// Each field mirrors an [`AgentFeature`] row. This descriptor — produced by
/// [`capabilities`] — is the **single source of truth** for the support matrix:
/// [`support`] derives from it rather than declaring the matrix a second time, so
/// the two can never drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentCapabilities {
    /// The unified `usagi` MCP server (issue / memory / session tools).
    pub mcp: bool,
    /// The `usagi-llm` MCP server for delegating light work to a local LLM.
    pub local_llm_mcp: bool,
    /// Lifecycle phase reporting (ready / running / waiting / ended / exited).
    pub phase_reporting: bool,
    /// Starting the session already working on a queued `session_prompt`.
    pub initial_prompt: bool,
    /// Injecting usagi's session system prompt (worktree note + delegation nudge).
    pub system_prompt: bool,
    /// Resuming the worktree's previous conversation on relaunch.
    pub resume: bool,
    /// Discarding the worktree's conversation history on `session remove`.
    pub forget_history: bool,
}

impl AgentCapabilities {
    /// Whether this descriptor grants `feature` — the bridge from the struct's
    /// fields back to an [`AgentFeature`] row, so callers can look a feature up
    /// dynamically. The `match` is exhaustive over [`AgentFeature`], so adding a
    /// feature is a compile error until this maps it to a field.
    pub fn has(self, feature: AgentFeature) -> bool {
        match feature {
            AgentFeature::Mcp => self.mcp,
            AgentFeature::LocalLlmMcp => self.local_llm_mcp,
            AgentFeature::PhaseReporting => self.phase_reporting,
            AgentFeature::InitialPrompt => self.initial_prompt,
            AgentFeature::SystemPrompt => self.system_prompt,
            AgentFeature::Resume => self.resume,
            AgentFeature::ForgetHistory => self.forget_history,
        }
    }

    /// This descriptor's [`Support`] for `feature` as a matrix cell.
    pub fn support(self, feature: AgentFeature) -> Support {
        if self.has(feature) {
            Support::Yes
        } else {
            Support::No
        }
    }
}

/// The capability descriptor for `cli` — the single source of truth for the
/// support matrix.
///
/// Claude, Codex, and SakanaAi receive every integration (Claude via its native
/// flags; Codex and SakanaAi via `-c` config overrides and Codex's
/// hook system). Gemini and Antigravity (`agy`, the Gemini CLI's successor) get
/// MCP through their provisioned MCP config and support the plain flags usagi can
/// drive — an opening prompt, resume, and the conversation store usagi can clear
/// — but lack an inline system-prompt injection flag and a usagi-drivable hook
/// system, so phase reporting and system-prompt injection stay unsupported.
///
/// The `match` is exhaustive over [`AgentCli`], so adding a variant is a compile
/// error until its capabilities are declared here.
pub fn capabilities(cli: AgentCli) -> AgentCapabilities {
    match cli {
        AgentCli::Claude | AgentCli::Codex | AgentCli::SakanaAi => AgentCapabilities {
            mcp: true,
            local_llm_mcp: true,
            phase_reporting: true,
            initial_prompt: true,
            system_prompt: true,
            resume: true,
            forget_history: true,
        },
        AgentCli::Gemini | AgentCli::Antigravity => AgentCapabilities {
            mcp: true,
            local_llm_mcp: true,
            phase_reporting: false,
            initial_prompt: true,
            system_prompt: false,
            resume: true,
            forget_history: true,
        },
    }
}

/// How `cli` supports `feature` through usagi — the support matrix cell, derived
/// from [`capabilities`] so the matrix stays declared in exactly one place.
pub fn support(cli: AgentCli, feature: AgentFeature) -> Support {
    capabilities(cli).support(feature)
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
        assert_eq!(Support::Yes.glyph(), "✓");
        assert_eq!(Support::No.glyph(), "—");
        assert_ne!(Support::Yes.glyph(), Support::No.glyph());
    }

    #[test]
    fn claude_and_codex_support_every_feature() {
        for feature in AgentFeature::ALL {
            assert_eq!(support(AgentCli::Claude, feature), Support::Yes);
            assert_eq!(support(AgentCli::Codex, feature), Support::Yes);
            // SakanaAi reuses the Codex adapter, so it supports the same set.
            assert_eq!(support(AgentCli::SakanaAi, feature), Support::Yes);
        }
    }

    #[test]
    fn capability_descriptor_is_the_single_source_for_support() {
        for cli in AgentCli::ALL {
            let descriptor = capabilities(cli);
            for feature in AgentFeature::ALL {
                assert_eq!(
                    support(cli, feature),
                    descriptor.support(feature),
                    "{cli:?} / {feature:?} should be derived from its descriptor"
                );
            }
        }
    }

    #[test]
    fn every_agent_cli_has_a_capability_descriptor() {
        // Iterating AgentCli::ALL is the runtime coverage check. The exhaustive
        // match in `capabilities` is the compile-time guard that catches a newly
        // added variant before this test can even build.
        assert_eq!(AgentCli::ALL.len(), 5);
        for cli in AgentCli::ALL {
            let descriptor = capabilities(cli);
            // Each current CLI can be launched with an opening prompt and can
            // clear/resume its own history. These smoke assertions make sure the
            // descriptor is not an all-false placeholder.
            assert!(descriptor.initial_prompt, "{cli:?}");
            assert!(descriptor.resume, "{cli:?}");
            assert!(descriptor.forget_history, "{cli:?}");
        }
    }

    #[test]
    fn gemini_supports_mcp_and_plain_flag_features() {
        assert_eq!(support(AgentCli::Gemini, AgentFeature::Mcp), Support::Yes);
        assert_eq!(
            support(AgentCli::Gemini, AgentFeature::LocalLlmMcp),
            Support::Yes
        );
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
        assert_eq!(
            support(AgentCli::Gemini, AgentFeature::PhaseReporting),
            Support::No
        );
        assert_eq!(
            support(AgentCli::Gemini, AgentFeature::SystemPrompt),
            Support::No
        );
    }

    #[test]
    fn antigravity_supports_the_same_features_as_gemini() {
        // `agy` shares Gemini's constraint (no inline injection, no hook system),
        // so it receives exactly the feature set Gemini does.
        for feature in AgentFeature::ALL {
            assert_eq!(
                support(AgentCli::Antigravity, feature),
                support(AgentCli::Gemini, feature),
                "antigravity should match gemini for {feature:?}"
            );
        }
    }
}

//! What an injected MCP wiring exposes to one agent launch.
//!
//! This module owns the single rule that turns an effective configuration into
//! tool families. Two consumers read it and must never disagree: the MCP server
//! builds its tool registry from it, and the launch prompt describes the same
//! families to the agent. Duplicating the rule is what lets `tools/list` and the
//! prompt drift apart.
//!
//! Resolving *which* configuration is effective stays with the caller: the MCP
//! server answers for its own cwd or the trusted root the daemon handed it, and
//! the daemon answers for the registered workspace root. That difference is
//! deliberate, so it is not folded in here.

use crate::domain::settings::Settings;

/// The tool families one injected MCP wiring exposes.
///
/// Prompt callers pass this as `Option`: `None` means no MCP server is wired, so
/// no tool is described at all. A disabled family is never mentioned — neither as
/// available nor as missing — because the registry that omits it is built from
/// this same value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpToolFamilies {
    /// The `issue_*` family (and `session_delegate_issue`) is registered.
    pub issue: bool,
    /// The `memory_*` family is registered.
    pub memory: bool,
    /// The trusted local-LLM server is wired next to the usagi server. It is a
    /// separate server, so it adds a tool to the launch without adding one to
    /// the usagi registry.
    pub local_llm: bool,
}

impl McpToolFamilies {
    /// The families `settings` exposes.
    ///
    /// `settings` must already be the effective configuration — the Global
    /// baseline with the workspace layer applied ([`Settings::with_local`]).
    /// Passing an unresolved baseline silently drops workspace overrides.
    #[must_use]
    pub const fn from_settings(settings: &Settings) -> Self {
        Self {
            issue: settings.issue_enabled,
            memory: settings.memory_enabled,
            local_llm: settings.local_llm.enabled,
        }
    }

    /// Every family enabled. This is the shape of a default install, and the one
    /// the registry validation and its tests exercise.
    #[must_use]
    pub const fn all() -> Self {
        Self {
            issue: true,
            memory: true,
            local_llm: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::settings::{LocalLlm, LocalSettings};

    #[test]
    fn the_effective_configuration_decides_every_family() {
        assert_eq!(
            McpToolFamilies::from_settings(&Settings::default()),
            McpToolFamilies {
                issue: true,
                memory: true,
                local_llm: false,
            }
        );

        let settings = Settings {
            memory_enabled: false,
            local_llm: LocalLlm {
                enabled: true,
                ..LocalLlm::default()
            },
            ..Settings::default()
        };
        assert_eq!(
            McpToolFamilies::from_settings(&settings),
            McpToolFamilies {
                issue: true,
                memory: false,
                local_llm: true,
            }
        );
    }

    #[test]
    fn the_workspace_layer_reaches_the_families_through_the_effective_settings() {
        // The rule reads one resolved value, so a caller that forgets
        // `with_local` loses the workspace layer instead of getting a second,
        // divergent rule.
        let local = LocalSettings {
            issue_enabled: Some(false),
            ..LocalSettings::default()
        };
        let effective = Settings::default().with_local(&local);
        assert!(!McpToolFamilies::from_settings(&effective).issue);
        assert!(McpToolFamilies::from_settings(&Settings::default()).issue);
    }

    #[test]
    fn all_enables_every_family() {
        assert_eq!(
            McpToolFamilies::all(),
            McpToolFamilies {
                issue: true,
                memory: true,
                local_llm: true,
            }
        );
    }
}

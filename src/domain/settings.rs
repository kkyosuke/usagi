use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// UI color theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    Light,
    Dark,
    /// Follow the OS appearance.
    #[default]
    System,
}

/// The AI agent CLI usagi drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentCli {
    /// Anthropic's Claude Code CLI.
    #[default]
    Claude,
    /// Google's Gemini CLI.
    Gemini,
}

/// JSON wiring usagi's own issue MCP server (`usagi mcp`, served over stdio)
/// into an agent CLI, so the agent can create and query issues from the start.
/// Kept as a literal — it is fixed and lets `domain` stay free of `serde_json`.
const ISSUE_MCP_CONFIG: &str = r#"{"mcpServers":{"usagi":{"command":"usagi","args":["mcp"]}}}"#;

/// System-prompt addendum injected into agents launched from a usagi session.
///
/// Every agent `:agent` starts already lives inside the session's dedicated
/// worktree, so the usual "create a worktree first" workflow step is redundant
/// here. We tell the agent up front to skip it and work in place. Kept free of
/// single quotes so it survives the single-quoted shell argument verbatim.
const SESSION_WORKTREE_PROMPT: &str = "あなたは usagi が管理するセッション専用の worktree 内で起動されています。このディレクトリは既に独立した作業環境のため、新たに git worktree を作成する必要はありません。ここで直接作業を進めてください。";

impl AgentCli {
    /// The shell command (program name) usagi launches for this agent — the
    /// word the `agent` command runs inside the embedded terminal.
    pub fn command(self) -> &'static str {
        match self {
            AgentCli::Claude => "claude",
            AgentCli::Gemini => "gemini",
        }
    }

    /// The full command line `:agent` sends to the embedded shell, with usagi's
    /// issue MCP server wired in so the agent can manage issues immediately.
    ///
    /// Claude Code accepts the server inline via `--mcp-config` and a
    /// session-scoped instruction via `--append-system-prompt`; both arguments
    /// are single-quoted so the shell passes them through verbatim (neither
    /// value contains a single quote). The system prompt tells the agent it is
    /// already inside a usagi worktree, so it skips creating one. Gemini has no
    /// inline flags — its MCP servers come from `settings.json` — so it launches
    /// plain for now.
    pub fn launch_command(self) -> String {
        match self {
            AgentCli::Claude => format!(
                "claude --mcp-config '{ISSUE_MCP_CONFIG}' \
                 --append-system-prompt '{SESSION_WORKTREE_PROMPT}'"
            ),
            AgentCli::Gemini => "gemini".to_string(),
        }
    }
}

/// Which ref a new session worktree is branched from.
///
/// When a session is created, each repository's worktree is cut on a new branch
/// off this base. The choice is project-local (see [`LocalSettings`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchSource {
    /// Branch off the repository's local default branch (e.g. `main`).
    Local,
    /// Branch off the remote-tracking default branch (e.g. `origin/main`), so
    /// sessions start from what has landed on the remote.
    #[default]
    Remote,
}

/// User-configurable application settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: Theme,
    /// Name of the workspace to open by default, if any.
    pub default_workspace: Option<String>,
    /// Base directory new projects are cloned under, if configured.
    ///
    /// When unset the New Project screen falls back to `~/git`.
    pub workspace_root: Option<PathBuf>,
    /// Whether desktop notifications are shown (e.g. on `hop`).
    pub notifications_enabled: bool,
    /// Which agent CLI usagi drives.
    pub agent_cli: AgentCli,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: Theme::default(),
            default_workspace: None,
            workspace_root: None,
            // Notifications are opt-out: on unless the user disables them.
            notifications_enabled: true,
            agent_cli: AgentCli::default(),
        }
    }
}

impl Settings {
    /// Apply a project's [`LocalSettings`] over these global settings, returning
    /// the effective settings for that project.
    ///
    /// Each local field overrides its global counterpart only when set; an unset
    /// (`None`) local field leaves the global value untouched.
    pub fn with_local(mut self, local: &LocalSettings) -> Self {
        if let Some(agent_cli) = local.agent_cli {
            self.agent_cli = agent_cli;
        }
        if let Some(notifications_enabled) = local.notifications_enabled {
            self.notifications_enabled = notifications_enabled;
        }
        self
    }
}

/// Per-project overrides of selected [`Settings`], stored alongside a
/// repository in `<repo>/.usagi/settings.json`.
///
/// Every field is optional: `None` means "defer to the global setting". Only
/// the settings that make sense to vary per project are represented here.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalSettings {
    /// Override which agent CLI usagi drives for this project.
    pub agent_cli: Option<AgentCli>,
    /// Override whether desktop notifications are shown for this project.
    pub notifications_enabled: Option<bool>,
    /// Which ref new session worktrees branch from in this repository. `None`
    /// defers to the default ([`BranchSource::Remote`]).
    pub default_branch_source: Option<BranchSource>,
}

impl LocalSettings {
    /// Whether every field is unset, i.e. the project adds no local override.
    pub fn is_empty(&self) -> bool {
        self.agent_cli.is_none()
            && self.notifications_enabled.is_none()
            && self.default_branch_source.is_none()
    }

    /// The branch source to use, resolving an unset value to the default.
    pub fn branch_source(&self) -> BranchSource {
        self.default_branch_source.unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_local_overrides_only_the_set_fields() {
        let global = Settings::default(); // agent_cli = Claude, notifications = true
        let local = LocalSettings {
            agent_cli: Some(AgentCli::Gemini),
            notifications_enabled: None,
            default_branch_source: None,
        };

        let effective = global.with_local(&local);

        // The set field is overridden...
        assert_eq!(effective.agent_cli, AgentCli::Gemini);
        // ...while the unset field keeps the global value.
        assert!(effective.notifications_enabled);
    }

    #[test]
    fn with_local_overrides_notifications_when_set() {
        let global = Settings::default();
        let local = LocalSettings {
            agent_cli: None,
            notifications_enabled: Some(false),
            default_branch_source: None,
        };

        let effective = global.with_local(&local);

        assert_eq!(effective.agent_cli, AgentCli::Claude);
        assert!(!effective.notifications_enabled);
    }

    #[test]
    fn with_local_is_a_no_op_when_empty() {
        let global = Settings::default();
        assert_eq!(global.clone().with_local(&LocalSettings::default()), global);
    }

    #[test]
    fn is_empty_reflects_whether_any_field_is_set() {
        assert!(LocalSettings::default().is_empty());
        assert!(!LocalSettings {
            agent_cli: Some(AgentCli::Claude),
            notifications_enabled: None,
            default_branch_source: None,
        }
        .is_empty());
        assert!(!LocalSettings {
            agent_cli: None,
            notifications_enabled: Some(true),
            default_branch_source: None,
        }
        .is_empty());
        // The branch source counts as an override too.
        assert!(!LocalSettings {
            agent_cli: None,
            notifications_enabled: None,
            default_branch_source: Some(BranchSource::Local),
        }
        .is_empty());
    }

    #[test]
    fn agent_cli_maps_to_its_launch_command() {
        assert_eq!(AgentCli::Claude.command(), "claude");
        assert_eq!(AgentCli::Gemini.command(), "gemini");
    }

    #[test]
    fn claude_launch_command_wires_in_the_issue_mcp_server() {
        let launch = AgentCli::Claude.launch_command();
        // The program is still `claude`, now with the issue MCP server passed
        // inline via `--mcp-config` and a session-scoped instruction passed via
        // `--append-system-prompt` (both single-quoted so the shell keeps them).
        assert_eq!(
            launch,
            "claude --mcp-config '{\"mcpServers\":{\"usagi\":{\"command\":\"usagi\",\"args\":[\"mcp\"]}}}' \
             --append-system-prompt 'あなたは usagi が管理するセッション専用の worktree 内で起動されています。このディレクトリは既に独立した作業環境のため、新たに git worktree を作成する必要はありません。ここで直接作業を進めてください。'"
        );
    }

    #[test]
    fn gemini_launch_command_stays_plain() {
        // Gemini has no inline MCP flag, so it launches as the bare command.
        assert_eq!(AgentCli::Gemini.launch_command(), "gemini");
    }

    #[test]
    fn branch_source_defaults_to_remote_when_unset() {
        // An unset override resolves to the default (Remote)...
        assert_eq!(
            LocalSettings::default().branch_source(),
            BranchSource::Remote
        );
        assert_eq!(BranchSource::default(), BranchSource::Remote);
        // ...and a set value is returned as-is.
        assert_eq!(
            LocalSettings {
                default_branch_source: Some(BranchSource::Local),
                ..Default::default()
            }
            .branch_source(),
            BranchSource::Local
        );
    }

    #[test]
    fn branch_source_serializes_in_snake_case() {
        assert_eq!(
            serde_json::to_string(&BranchSource::Local).unwrap(),
            "\"local\""
        );
        assert_eq!(
            serde_json::to_string(&BranchSource::Remote).unwrap(),
            "\"remote\""
        );
        assert_eq!(
            serde_json::from_str::<BranchSource>("\"remote\"").unwrap(),
            BranchSource::Remote
        );
    }
}

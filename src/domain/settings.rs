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

/// How the **在席 (Focus)** mode presents a session's runnable commands in the
/// right pane: as a pickable menu, or as a typed command prompt.
///
/// In the home screen's Focus mode the right pane is the session's action
/// surface. `Menu` lists the runnable commands (`terminal` / `agent`) for the
/// user to pick; `Prompt` offers a session-scoped command line to type into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionActionUi {
    /// A pickable list of the session's runnable commands (the default).
    #[default]
    Menu,
    /// A session-scoped command prompt the user types into.
    Prompt,
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

/// System-prompt addendum added when a local LLM MCP server is wired in.
///
/// It nudges the cloud agent to offload light, low-stakes work (summaries,
/// naming, boilerplate, simple transforms) to the `local_llm_ask` tool so the
/// cloud model's tokens are spent on the work that actually needs it. Kept free
/// of single quotes so it survives the single-quoted shell argument verbatim.
const LOCAL_LLM_PROMPT: &str = "トークン節約のため、要約・命名・定型文の生成・単純な変換といった軽量で重要度の低いタスクは、MCP ツール local_llm_ask（ローカル LLM）に委譲してください。判断が必要な作業や重要な実装はあなた自身が行ってください。";

/// The local LLM models usagi can delegate work to, in the order the config
/// screen cycles through them. All are Qwen variants pullable with
/// `ollama pull <model>`; the coder variants are tuned for code/technical
/// tasks, with smaller sizes for lower-spec machines.
pub const LOCAL_LLM_MODELS: [&str; 4] = [
    "qwen2.5-coder:7b",
    "qwen2.5-coder:3b",
    "qwen2.5-coder:1.5b",
    "qwen2.5:7b",
];

/// The model selected by default — the most capable coder variant in
/// [`LOCAL_LLM_MODELS`].
pub const DEFAULT_LOCAL_LLM_MODEL: &str = LOCAL_LLM_MODELS[0];

/// Configuration for the optional local LLM the agent can offload work to.
///
/// Disabled by default: usagi never enables it automatically. When enabled, the
/// chosen `model` is served to the agent through usagi's local LLM MCP server
/// (`usagi llm-mcp`) so the cloud agent can delegate light tasks to it and spend
/// fewer of its own tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalLlm {
    /// Whether the local LLM MCP server is wired into launched agents.
    pub enabled: bool,
    /// The Ollama model name delegated work runs against (e.g.
    /// `qwen2.5-coder:7b`).
    pub model: String,
}

impl Default for LocalLlm {
    fn default() -> Self {
        Self {
            // Opt-in: off until the user turns it on (and installs the model).
            enabled: false,
            model: DEFAULT_LOCAL_LLM_MODEL.to_string(),
        }
    }
}

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
    /// issue MCP server wired in so the agent can manage issues immediately —
    /// plus the local LLM MCP server when `local_llm_model` is `Some` (i.e. the
    /// local LLM is enabled), so the agent can offload light work to it.
    ///
    /// Claude Code accepts the servers inline via `--mcp-config` and a
    /// session-scoped instruction via `--append-system-prompt`; both arguments
    /// are single-quoted so the shell passes them through verbatim (neither
    /// value contains a single quote). The system prompt tells the agent it is
    /// already inside a usagi worktree, so it skips creating one, and — when the
    /// local LLM is on — to delegate light tasks to it. Gemini has no inline
    /// flags — its MCP servers come from `settings.json` — so it launches plain
    /// for now.
    pub fn launch_command(self, local_llm_model: Option<&str>) -> String {
        match self {
            AgentCli::Claude => {
                let mcp_config = mcp_config_json(local_llm_model);
                let system_prompt = match local_llm_model {
                    Some(_) => format!("{SESSION_WORKTREE_PROMPT}{LOCAL_LLM_PROMPT}"),
                    None => SESSION_WORKTREE_PROMPT.to_string(),
                };
                format!(
                    "claude --mcp-config '{mcp_config}' \
                     --append-system-prompt '{system_prompt}'"
                )
            }
            AgentCli::Gemini => "gemini".to_string(),
        }
    }
}

/// The `--mcp-config` JSON for Claude Code: always the issue server, plus the
/// local LLM server (`usagi llm-mcp --model <model>`) when a model is given.
///
/// Built by string formatting rather than `serde_json` so `domain` stays free
/// of that dependency; the model name comes from a fixed allowlist
/// ([`LOCAL_LLM_MODELS`]) with no characters that need JSON escaping.
fn mcp_config_json(local_llm_model: Option<&str>) -> String {
    match local_llm_model {
        None => ISSUE_MCP_CONFIG.to_string(),
        Some(model) => format!(
            r#"{{"mcpServers":{{"usagi":{{"command":"usagi","args":["mcp"]}},"usagi-llm":{{"command":"usagi","args":["llm-mcp","--model","{model}"]}}}}}}"#
        ),
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
    /// How the home screen's 在席 (Focus) mode presents a session's runnable
    /// commands in the right pane.
    pub session_action_ui: SessionActionUi,
    /// The optional local LLM the agent can offload light work to.
    pub local_llm: LocalLlm,
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
            session_action_ui: SessionActionUi::default(),
            local_llm: LocalLlm::default(),
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
        if let Some(local_llm_enabled) = local.local_llm_enabled {
            self.local_llm.enabled = local_llm_enabled;
        }
        self
    }

    /// The command line that launches the configured agent CLI with usagi's MCP
    /// servers wired in: always the issue server, plus the local LLM server when
    /// [`LocalLlm::enabled`] is set (so the agent can offload work to it).
    pub fn agent_launch_command(&self) -> String {
        let model = self
            .local_llm
            .enabled
            .then_some(self.local_llm.model.as_str());
        self.agent_cli.launch_command(model)
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
    /// Override whether the local LLM is enabled for this project. `None` defers
    /// to the global [`LocalLlm::enabled`] setting.
    pub local_llm_enabled: Option<bool>,
}

impl LocalSettings {
    /// Whether every field is unset, i.e. the project adds no local override.
    pub fn is_empty(&self) -> bool {
        self.agent_cli.is_none()
            && self.notifications_enabled.is_none()
            && self.default_branch_source.is_none()
            && self.local_llm_enabled.is_none()
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
            ..Default::default()
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
            notifications_enabled: Some(false),
            ..Default::default()
        };

        let effective = global.with_local(&local);

        assert_eq!(effective.agent_cli, AgentCli::Claude);
        assert!(!effective.notifications_enabled);
    }

    #[test]
    fn with_local_overrides_the_local_llm_toggle_when_set() {
        // The global default leaves the local LLM off; a local override turns it
        // on for just this project (the model is untouched).
        let global = Settings::default();
        assert!(!global.local_llm.enabled);
        let local = LocalSettings {
            local_llm_enabled: Some(true),
            ..Default::default()
        };

        let effective = global.with_local(&local);

        assert!(effective.local_llm.enabled);
        assert_eq!(effective.local_llm.model, DEFAULT_LOCAL_LLM_MODEL);
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
            ..Default::default()
        }
        .is_empty());
        assert!(!LocalSettings {
            notifications_enabled: Some(true),
            ..Default::default()
        }
        .is_empty());
        // The branch source counts as an override too.
        assert!(!LocalSettings {
            default_branch_source: Some(BranchSource::Local),
            ..Default::default()
        }
        .is_empty());
        // So does the local LLM toggle.
        assert!(!LocalSettings {
            local_llm_enabled: Some(false),
            ..Default::default()
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
        // With the local LLM off (`None`), only the issue server is wired in and
        // the system prompt is just the worktree note.
        let launch = AgentCli::Claude.launch_command(None);
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
    fn claude_launch_command_wires_in_the_local_llm_server_when_enabled() {
        // With a model given, the local LLM server joins the issue server in the
        // MCP config and the delegation prompt is appended after the worktree note.
        let launch = AgentCli::Claude.launch_command(Some("qwen2.5-coder:7b"));
        assert!(launch.contains(
            "\"usagi-llm\":{\"command\":\"usagi\",\"args\":[\"llm-mcp\",\"--model\",\"qwen2.5-coder:7b\"]}"
        ));
        // The issue server is still present alongside it.
        assert!(launch.contains("\"usagi\":{\"command\":\"usagi\",\"args\":[\"mcp\"]}"));
        // The delegation instruction is appended to the worktree note.
        assert!(launch.contains("local_llm_ask"));
    }

    #[test]
    fn gemini_launch_command_stays_plain_regardless_of_local_llm() {
        // Gemini has no inline MCP flag, so it launches as the bare command even
        // when the local LLM is enabled.
        assert_eq!(AgentCli::Gemini.launch_command(None), "gemini");
        assert_eq!(
            AgentCli::Gemini.launch_command(Some("qwen2.5-coder:7b")),
            "gemini"
        );
    }

    #[test]
    fn agent_launch_command_wires_the_local_llm_only_when_enabled() {
        // Disabled (the default): no local LLM server, no delegation prompt.
        let mut settings = Settings::default();
        let off = settings.agent_launch_command();
        assert!(!off.contains("usagi-llm"));
        assert!(!off.contains("local_llm_ask"));

        // Enabled: the configured model is served and the prompt is added.
        settings.local_llm.enabled = true;
        settings.local_llm.model = "qwen2.5-coder:3b".to_string();
        let on = settings.agent_launch_command();
        assert!(on.contains("\"--model\",\"qwen2.5-coder:3b\""));
        assert!(on.contains("local_llm_ask"));
    }

    #[test]
    fn local_llm_defaults_to_off_with_the_default_model() {
        let local_llm = LocalLlm::default();
        assert!(!local_llm.enabled);
        assert_eq!(local_llm.model, "qwen2.5-coder:7b");
        assert_eq!(DEFAULT_LOCAL_LLM_MODEL, LOCAL_LLM_MODELS[0]);
        // The default model is one of the offered choices.
        assert!(LOCAL_LLM_MODELS.contains(&DEFAULT_LOCAL_LLM_MODEL));
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

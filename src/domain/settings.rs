use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::domain::agent::AgentWiring;

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

/// The always-present usagi MCP server wired into an agent CLI, as the inner
/// body of the `"mcpServers"` object (no enclosing braces): the unified `usagi`
/// server (`<usagi_bin> mcp`) so the agent can create and query issues, save and
/// recall memories, and create sessions and delegate prompts to them — all from
/// one server.
///
/// `usagi_bin` is the command the agent uses to invoke usagi — the absolute path
/// of the running binary (see [`AgentCli::launch_command`]), so it resolves
/// whether usagi is installed on `$PATH` or run straight from a build
/// (`cargo run`, where the binary is `target/debug/usagi`, not on `$PATH`). The
/// path is JSON-escaped via [`json_escape`].
fn usagi_mcp_servers(usagi_bin: &str) -> String {
    let bin = json_escape(usagi_bin);
    format!(r#""usagi":{{"command":"{bin}","args":["mcp"]}}"#)
}

/// JSON wiring Claude Code's lifecycle hooks back into usagi, so the agent
/// reports its own ready / running / waiting state instead of usagi guessing from
/// the terminal bell. Each hook runs `<usagi_bin> agent-phase <phase>`, which
/// records the phase for the worktree the agent runs in (the hook delivers its
/// `cwd` on stdin); the home screen's session watcher reads it back to mark the
/// session. `usagi_bin` is the resolved usagi binary path (see
/// [`usagi_mcp_servers`]).
///
/// The events: a freshly started or resumed session is idle (`SessionStart` →
/// `ready`); a submitted prompt starts a turn (`UserPromptSubmit` → `running`);
/// finishing a turn means the agent is done (`Stop` → `ended`); pausing mid-turn
/// for the user's input or permission means it waits (`Notification` →
/// `waiting`); the session ending is also done (`SessionEnd` → `ended`). Passed
/// via `--settings`, which *merges* with the user's own settings rather than
/// replacing them. Built by string formatting (not `serde_json`) to keep `domain`
/// dependency-free; the binary path is JSON-escaped so a Windows path stays valid
/// JSON, and contains only double quotes so it survives the single-quoted shell
/// argument.
fn claude_hooks_settings(usagi_bin: &str) -> String {
    let bin = json_escape(usagi_bin);
    format!(
        r#"{{"hooks":{{"UserPromptSubmit":[{{"hooks":[{{"type":"command","command":"{bin} agent-phase running"}}]}}],"Stop":[{{"hooks":[{{"type":"command","command":"{bin} agent-phase ended"}}]}}],"Notification":[{{"hooks":[{{"type":"command","command":"{bin} agent-phase waiting"}}]}}],"SessionStart":[{{"hooks":[{{"type":"command","command":"{bin} agent-phase ready"}}]}}],"SessionEnd":[{{"hooks":[{{"type":"command","command":"{bin} agent-phase ended"}}]}}]}}}}"#
    )
}

/// Escape a string for embedding as a JSON string value: double the backslashes
/// and escape the double quotes. Keeps the formatted MCP / hooks JSON valid when
/// the usagi binary path contains backslashes (a Windows path like
/// `C:\…\usagi.exe`) or quotes, without pulling `serde_json` into `domain`.
fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

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
    /// Claude Code accepts the servers inline via `--mcp-config`, a
    /// session-scoped instruction via `--append-system-prompt`, and lifecycle
    /// hooks via `--settings` (see [`claude_hooks_settings`], so the agent reports
    /// its own running / waiting state); all three arguments are single-quoted so
    /// the shell passes them through verbatim (no value contains a single quote).
    /// The system prompt tells the agent it is already inside a usagi worktree,
    /// so it skips creating one, and — when the local LLM is on — to delegate
    /// light tasks to it. Gemini has no inline flags — its MCP servers come from
    /// `settings.json` — so it launches plain for now.
    ///
    /// `usagi_bin` is the command the agent uses to invoke usagi back (for the
    /// MCP servers and lifecycle hooks): the absolute path of the running binary,
    /// resolved by the caller via `std::env::current_exe()`. Passing the resolved
    /// path rather than the bare name `usagi` makes the wiring work even when
    /// usagi is run straight from a build (`cargo run`) and is not on `$PATH`.
    ///
    /// When `resume` is set, Claude is launched with `--continue` so it picks up
    /// the worktree's previous conversation instead of starting fresh — usagi
    /// only sets it when such a conversation exists (see
    /// [`Agent::has_resumable_session`](crate::domain::agent::Agent::has_resumable_session)).
    /// Gemini has no resume flag, so it ignores `resume` and launches plain.
    pub fn launch_command(
        self,
        local_llm_model: Option<&str>,
        usagi_bin: &str,
        resume: bool,
    ) -> String {
        match self {
            AgentCli::Claude => {
                let mcp_config = mcp_config_json(local_llm_model, usagi_bin);
                let system_prompt = match local_llm_model {
                    Some(_) => format!("{SESSION_WORKTREE_PROMPT}{LOCAL_LLM_PROMPT}"),
                    None => SESSION_WORKTREE_PROMPT.to_string(),
                };
                let hooks = claude_hooks_settings(usagi_bin);
                // `--continue` resumes the most recent conversation in the
                // worktree; placed right after the program name so it reads like
                // a plain `claude -c` with usagi's wiring appended.
                let resume_flag = if resume { "--continue " } else { "" };
                format!(
                    "claude {resume_flag}--mcp-config '{mcp_config}' \
                     --append-system-prompt '{system_prompt}' \
                     --settings '{hooks}'"
                )
            }
            AgentCli::Gemini => "gemini".to_string(),
        }
    }
}

/// The `--mcp-config` JSON for Claude Code: always the unified usagi server,
/// plus the local LLM server (`<usagi_bin> llm-mcp --model <model>`) when a model
/// is given. `usagi_bin` is the resolved usagi binary path (see
/// [`AgentCli::launch_command`]).
///
/// Built by string formatting rather than `serde_json` so `domain` stays free
/// of that dependency; the model name comes from a fixed allowlist
/// ([`LOCAL_LLM_MODELS`]) with no characters that need JSON escaping, and the
/// binary path is JSON-escaped via [`json_escape`].
fn mcp_config_json(local_llm_model: Option<&str>, usagi_bin: &str) -> String {
    let servers = usagi_mcp_servers(usagi_bin);
    let servers = match local_llm_model {
        None => servers,
        Some(model) => {
            let bin = json_escape(usagi_bin);
            format!(
                r#"{servers},"usagi-llm":{{"command":"{bin}","args":["llm-mcp","--model","{model}"]}}"#
            )
        }
    };
    format!(r#"{{"mcpServers":{{{servers}}}}}"#)
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

    /// usagi's wiring policy for a launched agent: the resolved usagi binary path
    /// the agent invokes back through (MCP servers and lifecycle hooks) and the
    /// local-LLM model to offload light work to, when [`LocalLlm::enabled`] is set.
    /// An [`Agent`](crate::domain::agent::Agent) adapter renders this into its
    /// CLI's own invocation.
    ///
    /// `usagi_bin` is the absolute path of the running binary
    /// (`std::env::current_exe()`), so the wiring resolves even when usagi is run
    /// from a build and not installed on `$PATH`. See [`AgentCli::launch_command`].
    pub fn agent_wiring(&self, usagi_bin: &str) -> AgentWiring {
        AgentWiring {
            usagi_bin: usagi_bin.to_string(),
            local_llm_model: self.local_llm.enabled.then(|| self.local_llm.model.clone()),
        }
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
    /// The branch new session worktrees are cut from in this repository. `None`
    /// means "use the repository's detected default branch" (e.g. `main`); a
    /// value names a specific branch (e.g. `develop`) to branch off instead. The
    /// [`default_branch_source`](Self::default_branch_source) still decides
    /// whether the local or remote-tracking form of that branch is used.
    pub default_branch: Option<String>,
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
            && self.default_branch.is_none()
            && self.local_llm_enabled.is_none()
    }

    /// The branch source to use, resolving an unset value to the default.
    pub fn branch_source(&self) -> BranchSource {
        self.default_branch_source.unwrap_or_default()
    }

    /// The specific branch new sessions should branch from, or `None` to use the
    /// repository's detected default branch.
    pub fn default_branch(&self) -> Option<&str> {
        self.default_branch.as_deref()
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
        // ...as does a specific default branch.
        assert!(!LocalSettings {
            default_branch: Some("develop".to_string()),
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
    fn claude_launch_command_wires_in_the_usagi_mcp_servers() {
        // With the local LLM off (`None`), the unified usagi server is wired in
        // and the system prompt is just the worktree note. The bare name `usagi`
        // stands in for the resolved binary path the caller passes.
        let launch = AgentCli::Claude.launch_command(None, "usagi", false);
        // The program is still `claude`, now with usagi's MCP server passed
        // inline via `--mcp-config` and a session-scoped instruction passed via
        // `--append-system-prompt` (both single-quoted so the shell keeps them).
        assert_eq!(
            launch,
            "claude --mcp-config '{\"mcpServers\":{\"usagi\":{\"command\":\"usagi\",\"args\":[\"mcp\"]}}}' \
             --append-system-prompt 'あなたは usagi が管理するセッション専用の worktree 内で起動されています。このディレクトリは既に独立した作業環境のため、新たに git worktree を作成する必要はありません。ここで直接作業を進めてください。' \
             --settings '{\"hooks\":{\"UserPromptSubmit\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"usagi agent-phase running\"}]}],\"Stop\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"usagi agent-phase ended\"}]}],\"Notification\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"usagi agent-phase waiting\"}]}],\"SessionStart\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"usagi agent-phase ready\"}]}],\"SessionEnd\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"usagi agent-phase ended\"}]}]}}'"
        );
    }

    #[test]
    fn claude_launch_command_adds_continue_only_when_resuming() {
        // Resuming inserts `--continue` right after the program name so Claude
        // picks up the worktree's previous conversation; the rest of the wiring
        // is unchanged.
        let resumed = AgentCli::Claude.launch_command(None, "usagi", true);
        assert!(resumed.starts_with("claude --continue --mcp-config '"));
        // Without resuming the flag is absent and the command starts plainly.
        let fresh = AgentCli::Claude.launch_command(None, "usagi", false);
        assert!(fresh.starts_with("claude --mcp-config '"));
        assert!(!fresh.contains("--continue"));
    }

    #[test]
    fn claude_launch_command_wires_in_the_local_llm_server_when_enabled() {
        // With a model given, the local LLM server joins the issue server in the
        // MCP config and the delegation prompt is appended after the worktree note.
        let launch = AgentCli::Claude.launch_command(Some("qwen2.5-coder:7b"), "usagi", false);
        assert!(launch.contains(
            "\"usagi-llm\":{\"command\":\"usagi\",\"args\":[\"llm-mcp\",\"--model\",\"qwen2.5-coder:7b\"]}"
        ));
        // The issue server is still present alongside it.
        assert!(launch.contains("\"usagi\":{\"command\":\"usagi\",\"args\":[\"mcp\"]}"));
        // The delegation instruction is appended to the worktree note.
        assert!(launch.contains("local_llm_ask"));
    }

    #[test]
    fn claude_launch_command_wires_in_lifecycle_hooks() {
        // The phase-reporting hooks ride along via --settings whether or not the
        // local LLM is enabled, so usagi always learns the agent's state.
        for model in [None, Some("qwen2.5-coder:7b")] {
            let launch = AgentCli::Claude.launch_command(model, "usagi", false);
            assert!(launch.contains("--settings '{\"hooks\":"));
            assert!(launch.contains("usagi agent-phase ready"));
            assert!(launch.contains("usagi agent-phase running"));
            assert!(launch.contains("usagi agent-phase waiting"));
            assert!(launch.contains("usagi agent-phase ended"));
        }
    }

    #[test]
    fn launch_command_embeds_the_given_binary_path_in_hooks_and_mcp() {
        // The caller passes the resolved usagi binary path (e.g. from
        // `current_exe()`); both the MCP servers and every lifecycle hook must
        // invoke that exact path, not the bare name `usagi`, so the wiring works
        // when usagi is run from a build that is not on `$PATH`.
        let launch = AgentCli::Claude.launch_command(
            Some("qwen2.5-coder:7b"),
            "/opt/usagi/bin/usagi",
            false,
        );
        // MCP servers point at the resolved binary.
        assert!(launch.contains(r#""usagi":{"command":"/opt/usagi/bin/usagi","args":["mcp"]}"#));
        assert!(launch.contains(
            r#""usagi-llm":{"command":"/opt/usagi/bin/usagi","args":["llm-mcp","--model","qwen2.5-coder:7b"]}"#
        ));
        // Every lifecycle hook invokes that same binary.
        assert!(launch.contains("/opt/usagi/bin/usagi agent-phase ready"));
        assert!(launch.contains("/opt/usagi/bin/usagi agent-phase running"));
        assert!(launch.contains("/opt/usagi/bin/usagi agent-phase waiting"));
        assert!(launch.contains("/opt/usagi/bin/usagi agent-phase ended"));
        // The bare name no longer appears as a standalone command.
        assert!(!launch.contains(r#""command":"usagi""#));
    }

    #[test]
    fn launch_command_json_escapes_a_windows_binary_path() {
        // A Windows path carries backslashes; they must be doubled so the
        // `--mcp-config` / `--settings` JSON stays valid.
        let launch = AgentCli::Claude.launch_command(None, r"C:\usagi\usagi.exe", false);
        assert!(launch.contains(r#""command":"C:\\usagi\\usagi.exe","args":["mcp"]"#));
        assert!(launch.contains(r"C:\\usagi\\usagi.exe agent-phase running"));
    }

    #[test]
    fn json_escape_doubles_backslashes_and_escapes_quotes() {
        assert_eq!(json_escape(r"C:\bin\usagi.exe"), r"C:\\bin\\usagi.exe");
        assert_eq!(json_escape(r#"a"b"#), r#"a\"b"#);
        // A plain path is returned unchanged.
        assert_eq!(json_escape("/usr/local/bin/usagi"), "/usr/local/bin/usagi");
    }

    #[test]
    fn gemini_launch_command_stays_plain_regardless_of_local_llm() {
        // Gemini has no inline MCP flag, so it launches as the bare command even
        // when the local LLM is enabled.
        assert_eq!(
            AgentCli::Gemini.launch_command(None, "usagi", false),
            "gemini"
        );
        assert_eq!(
            AgentCli::Gemini.launch_command(Some("qwen2.5-coder:7b"), "usagi", false),
            "gemini"
        );
        // The resume flag has no Gemini equivalent, so it stays plain.
        assert_eq!(
            AgentCli::Gemini.launch_command(None, "usagi", true),
            "gemini"
        );
    }

    #[test]
    fn agent_wiring_carries_the_binary_and_the_local_llm_model_only_when_enabled() {
        // Disabled (the default): the binary path is carried, the model is None.
        let mut settings = Settings::default();
        let off = settings.agent_wiring("/opt/usagi/bin/usagi");
        assert_eq!(off.usagi_bin, "/opt/usagi/bin/usagi");
        assert_eq!(off.local_llm_model, None);

        // Enabled: the configured model rides along for the adapter to wire in.
        settings.local_llm.enabled = true;
        settings.local_llm.model = "qwen2.5-coder:3b".to_string();
        let on = settings.agent_wiring("usagi");
        assert_eq!(on.local_llm_model.as_deref(), Some("qwen2.5-coder:3b"));
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
    fn default_branch_resolves_to_none_or_the_named_branch() {
        // Unset: use the repository's detected default branch.
        assert_eq!(LocalSettings::default().default_branch(), None);
        // Set: the named branch is returned.
        assert_eq!(
            LocalSettings {
                default_branch: Some("develop".to_string()),
                ..Default::default()
            }
            .default_branch(),
            Some("develop")
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

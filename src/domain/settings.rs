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
    /// OpenAI's Codex CLI.
    Codex,
    /// A Codex-compatible CLI invoked as `codex-fugu` (same invocation surface as
    /// Codex — `-c` overrides, lifecycle hooks, `resume --last` — with its own
    /// rollout store under `~/.codex-fugu`).
    CodexFugu,
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

/// How the home screen's left **session sidebar** is sized: spelled out at its
/// full width, or collapsed to a compact rail.
///
/// `Full` lists each session with its name, git status, and agent state. `Rail`
/// collapses the list to a narrow vertical strip — a gutter bar marking the
/// active session, a 1-based index, and the agent-state icon — giving the right
/// pane (notably the embedded terminal) more width while still showing which
/// session is active. `Ctrl-B` toggles between them at runtime; this setting is
/// only the state the screen opens in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sidebar {
    /// The full-width session list (the default).
    #[default]
    Full,
    /// The collapsed rail: a gutter bar, index, and agent-state icon per session.
    Rail,
}

impl Sidebar {
    /// The sidebar's state after a toggle (`Ctrl-B`): full ⇄ rail.
    pub fn toggled(self) -> Self {
        match self {
            Sidebar::Full => Sidebar::Rail,
            Sidebar::Rail => Sidebar::Full,
        }
    }
}

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
    /// Every agent CLI variant, in the canonical order menus, the config-screen
    /// selector, and the `usagi feature` table list them.
    pub const ALL: [AgentCli; 4] = [
        AgentCli::Claude,
        AgentCli::Codex,
        AgentCli::CodexFugu,
        AgentCli::Gemini,
    ];

    /// The shell command (program name) usagi launches for this agent — the
    /// word the `agent` command runs inside the embedded terminal.
    ///
    /// How the full launch command line is built (MCP servers, system prompt,
    /// lifecycle hooks) is the agent adapter's job, not the domain's — see the
    /// [`Agent`](crate::domain::agent::Agent) port and its
    /// `infrastructure::agent` implementations.
    pub fn command(self) -> &'static str {
        match self {
            AgentCli::Claude => "claude",
            AgentCli::Codex => "codex",
            AgentCli::CodexFugu => "codex-fugu",
            AgentCli::Gemini => "gemini",
        }
    }

    /// The human-facing display name shown wherever the CLI is presented to the
    /// user (the config-screen selector, the `usagi feature` table, `doctor`).
    /// Distinct from [`command`](Self::command): the codex-fugu variant launches
    /// `codex-fugu` but is presented as `sakana.ai`. The single source of truth
    /// for these labels.
    pub fn display_name(self) -> &'static str {
        match self {
            AgentCli::Claude => "Claude",
            AgentCli::Codex => "Codex",
            AgentCli::CodexFugu => "sakana.ai",
            AgentCli::Gemini => "Gemini",
        }
    }

    /// Resolve a user-typed agent name to its variant, accepting both the launch
    /// [`command`](Self::command) (`claude` / `codex` / `codex-fugu` / `gemini`)
    /// and the [`display_name`](Self::display_name) (`sakana.ai` for codex-fugu),
    /// case-insensitively. Used by the 在席 prompt's `agent <name>` to pick which
    /// CLI to launch. Returns `None` for an unrecognised name.
    pub fn from_name(name: &str) -> Option<AgentCli> {
        let name = name.trim().to_ascii_lowercase();
        AgentCli::ALL
            .into_iter()
            .find(|cli| cli.command() == name || cli.display_name().to_ascii_lowercase() == name)
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
    // Enum fields degrade an unrecognised stored value to their default rather
    // than failing the whole file (see [`crate::domain::serde_fallback`]), so a
    // newer usagi's value — or a hand-edited typo — never blocks loading every
    // other setting.
    #[serde(deserialize_with = "crate::domain::serde_fallback::or_default")]
    pub theme: Theme,
    /// Name of the workspace to open by default, if any.
    pub default_workspace: Option<String>,
    /// Base directory new projects are cloned under, if configured.
    ///
    /// When unset the New Project screen falls back to `~/git`.
    pub workspace_root: Option<PathBuf>,
    /// Whether desktop notifications are shown (e.g. on `hop`).
    pub notifications_enabled: bool,
    /// Whether the home screen restores each session's open panes (agent /
    /// terminal) on startup, re-spawning them in the background — an agent picks
    /// its conversation back up where it left off. On unless the user disables it.
    pub restore_panes_enabled: bool,
    /// Which agent CLI usagi drives.
    #[serde(deserialize_with = "crate::domain::serde_fallback::or_default")]
    pub agent_cli: AgentCli,
    /// How the home screen's 在席 (Focus) mode presents a session's runnable
    /// commands in the right pane.
    #[serde(deserialize_with = "crate::domain::serde_fallback::or_default")]
    pub session_action_ui: SessionActionUi,
    /// Which state the home screen's left session sidebar opens in (`Ctrl-B`
    /// toggles it at runtime).
    #[serde(deserialize_with = "crate::domain::serde_fallback::or_default")]
    pub sidebar: Sidebar,
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
            // Restoring open panes is opt-out: on unless the user disables it.
            restore_panes_enabled: true,
            agent_cli: AgentCli::default(),
            session_action_ui: SessionActionUi::default(),
            sidebar: Sidebar::default(),
            local_llm: LocalLlm::default(),
        }
    }
}

impl Settings {
    /// Coerce loaded settings into a trusted state, dropping values that did not
    /// come from usagi's own UI.
    ///
    /// The config screen only ever stores a `local_llm.model` from the fixed
    /// [`LOCAL_LLM_MODELS`] allowlist, but `settings.json` is a plain file a user
    /// (or a synced dotfiles repo) can hand-edit. The model name is later
    /// interpolated into the agent launch command, so an unexpected value — for
    /// instance one containing a shell single quote — must not be trusted
    /// verbatim. Any model outside the allowlist is reset to
    /// [`DEFAULT_LOCAL_LLM_MODEL`]. This is defense-in-depth: the launch builder
    /// also escapes its arguments, so neither layer alone is load-bearing.
    pub fn sanitized(mut self) -> Self {
        if !LOCAL_LLM_MODELS.contains(&self.local_llm.model.as_str()) {
            self.local_llm.model = DEFAULT_LOCAL_LLM_MODEL.to_string();
        }
        self
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
        if let Some(restore_panes_enabled) = local.restore_panes_enabled {
            self.restore_panes_enabled = restore_panes_enabled;
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
    /// from a build and not installed on `$PATH`. How an adapter renders this is
    /// the [`Agent`](crate::domain::agent::Agent) port's
    /// [`launch_command`](crate::domain::agent::Agent::launch_command).
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
    /// Override which agent CLI usagi drives for this project. An unrecognised
    /// stored value degrades to `None` (defer to the global setting) rather than
    /// failing the whole file — see [`crate::domain::serde_fallback`].
    #[serde(deserialize_with = "crate::domain::serde_fallback::or_default")]
    pub agent_cli: Option<AgentCli>,
    /// Override whether desktop notifications are shown for this project.
    pub notifications_enabled: Option<bool>,
    /// Override whether this project's session panes are restored on startup.
    /// `None` defers to the global setting.
    pub restore_panes_enabled: Option<bool>,
    /// Which ref new session worktrees branch from in this repository. `None`
    /// defers to the default ([`BranchSource::Remote`]). An unrecognised stored
    /// value degrades to `None`.
    #[serde(deserialize_with = "crate::domain::serde_fallback::or_default")]
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
            && self.restore_panes_enabled.is_none()
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
    fn with_local_overrides_restore_panes_when_set() {
        // The global default restores panes; a local override turns it off for
        // just this project.
        let global = Settings::default();
        assert!(global.restore_panes_enabled);
        let local = LocalSettings {
            restore_panes_enabled: Some(false),
            ..Default::default()
        };

        let effective = global.with_local(&local);

        assert!(!effective.restore_panes_enabled);
        // Unrelated fields keep their global value.
        assert!(effective.notifications_enabled);
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
        // As does the restore-panes toggle.
        assert!(!LocalSettings {
            restore_panes_enabled: Some(false),
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
    fn agent_cli_maps_to_its_program_command() {
        assert_eq!(AgentCli::Claude.command(), "claude");
        assert_eq!(AgentCli::Codex.command(), "codex");
        assert_eq!(AgentCli::CodexFugu.command(), "codex-fugu");
        assert_eq!(AgentCli::Gemini.command(), "gemini");
    }

    #[test]
    fn agent_cli_all_and_display_names_cover_every_variant() {
        // ALL lists each variant once, in canonical order.
        assert_eq!(
            AgentCli::ALL,
            [
                AgentCli::Claude,
                AgentCli::Codex,
                AgentCli::CodexFugu,
                AgentCli::Gemini
            ]
        );
        // Each has a non-empty display name; the codex-fugu variant shows as
        // `sakana.ai` even though its launch command is `codex-fugu`.
        for cli in AgentCli::ALL {
            assert!(!cli.display_name().is_empty());
        }
        assert_eq!(AgentCli::Claude.display_name(), "Claude");
        assert_eq!(AgentCli::Codex.display_name(), "Codex");
        assert_eq!(AgentCli::CodexFugu.display_name(), "sakana.ai");
        assert_eq!(AgentCli::Gemini.display_name(), "Gemini");
    }

    #[test]
    fn agent_cli_from_name_accepts_command_and_display_names_case_insensitively() {
        // The launch command resolves each variant.
        assert_eq!(AgentCli::from_name("claude"), Some(AgentCli::Claude));
        assert_eq!(AgentCli::from_name("codex"), Some(AgentCli::Codex));
        assert_eq!(AgentCli::from_name("codex-fugu"), Some(AgentCli::CodexFugu));
        assert_eq!(AgentCli::from_name("gemini"), Some(AgentCli::Gemini));
        // The display name resolves too — `sakana.ai` is codex-fugu's label.
        assert_eq!(AgentCli::from_name("sakana.ai"), Some(AgentCli::CodexFugu));
        // Case and surrounding whitespace are ignored.
        assert_eq!(AgentCli::from_name("  Claude "), Some(AgentCli::Claude));
        assert_eq!(AgentCli::from_name("SAKANA.AI"), Some(AgentCli::CodexFugu));
        // An unrecognised name resolves to nothing.
        assert_eq!(AgentCli::from_name("emacs"), None);
        assert_eq!(AgentCli::from_name(""), None);
    }

    #[test]
    fn agent_cli_serializes_in_snake_case() {
        // The persisted (on-disk) form is snake_case, so `CodexFugu` is stored as
        // `codex_fugu` even though the launched program is `codex-fugu`.
        assert_eq!(
            serde_json::to_string(&AgentCli::CodexFugu).unwrap(),
            "\"codex_fugu\""
        );
        assert_eq!(
            serde_json::from_str::<AgentCli>("\"codex_fugu\"").unwrap(),
            AgentCli::CodexFugu
        );
        assert_eq!(AgentCli::CodexFugu.command(), "codex-fugu");
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
    fn sanitized_resets_an_unknown_local_llm_model() {
        // A known model from the allowlist is preserved verbatim.
        let known = Settings {
            local_llm: LocalLlm {
                enabled: true,
                model: "qwen2.5-coder:3b".to_string(),
            },
            ..Default::default()
        };
        assert_eq!(
            known.clone().sanitized().local_llm.model,
            "qwen2.5-coder:3b"
        );

        // A model outside the allowlist — e.g. a hand-edited injection attempt —
        // is reset to the default, while every other field is left untouched.
        let tampered = Settings {
            theme: Theme::Dark,
            local_llm: LocalLlm {
                enabled: true,
                model: "evil';touch /tmp/pwned;'".to_string(),
            },
            ..Default::default()
        };
        let cleaned = tampered.sanitized();
        assert_eq!(cleaned.local_llm.model, DEFAULT_LOCAL_LLM_MODEL);
        assert!(cleaned.local_llm.enabled);
        assert_eq!(cleaned.theme, Theme::Dark);
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
    fn sidebar_defaults_to_full_and_serializes_in_snake_case() {
        // The screen opens with the full-width sidebar unless configured otherwise.
        assert_eq!(Sidebar::default(), Sidebar::Full);
        assert_eq!(Settings::default().sidebar, Sidebar::Full);
        // Round-trips through the snake_case JSON the rest of the settings use.
        assert_eq!(serde_json::to_string(&Sidebar::Full).unwrap(), "\"full\"");
        assert_eq!(serde_json::to_string(&Sidebar::Rail).unwrap(), "\"rail\"");
        assert_eq!(
            serde_json::from_str::<Sidebar>("\"rail\"").unwrap(),
            Sidebar::Rail
        );
    }

    #[test]
    fn sidebar_toggles_between_full_and_rail() {
        assert_eq!(Sidebar::Full.toggled(), Sidebar::Rail);
        assert_eq!(Sidebar::Rail.toggled(), Sidebar::Full);
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

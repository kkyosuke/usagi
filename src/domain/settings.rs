use std::collections::BTreeMap;
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

/// How the embedded terminal (**没入 / Attached**) reserves its own keys, so
/// everything else reaches the shell / agent running inside the pane.
///
/// The pane has to claim a few keys for its own navigation (switch tab, zoom
/// out, …). `Prefix` is the tmux / screen-style leader: `Ctrl-O` then a letter
/// runs the action, so `Ctrl-O` is the *only* chord the pane claims and every
/// other Ctrl key (`Ctrl-E` end-of-line, `Ctrl-N`/`Ctrl-P` history, …) flows to
/// the shell untouched. `Alt` instead binds each action to a single `Alt`-chord
/// (zellij-style): no bare Ctrl key is claimed and navigation stays one
/// keystroke, but the terminal must send `Alt`/`Option` as Meta (on macOS the
/// terminal's "Option as Meta" / "Esc+" setting). The per-scheme keymaps live in
/// the 没入 input layer (`presentation::tui::home::pane_input`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyScheme {
    /// `Ctrl-O` leader, then a key — the default. Claims only `Ctrl-O`; works on
    /// any terminal with no extra setup.
    #[default]
    Prefix,
    /// Single `Alt`-chords. Claims no bare Ctrl key and keeps navigation to one
    /// keystroke, but needs the terminal to deliver `Alt`/`Option` as Meta.
    Alt,
}

impl KeyScheme {
    /// Both schemes, in the order the config screen cycles through them.
    pub const ALL: [KeyScheme; 2] = [KeyScheme::Prefix, KeyScheme::Alt];
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

/// Configuration for the optional 1Password MCP server the agent can read
/// secrets through (`usagi op-mcp`).
///
/// Disabled by default: usagi never wires it automatically. Registering a
/// 1Password **service account token** here turns it on — the 1Password MCP
/// server is then wired into launched agents and authenticates `op` with that
/// token, so the agent can resolve secret references (`op://…`) on demand. The
/// token is read only by the `usagi op-mcp` process and used to set
/// `OP_SERVICE_ACCOUNT_TOKEN` for the `op` subprocess; it is never placed on a
/// command line.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OpMcp {
    /// The 1Password service account token `op` authenticates with. `None` (or a
    /// blank string) leaves the server unwired and falls back to whatever ambient
    /// `op` session exists.
    pub service_account_token: Option<String>,
}

impl OpMcp {
    /// Whether a non-blank service account token is registered — i.e. the
    /// 1Password MCP server should be wired into launched agents. A present but
    /// whitespace-only token counts as unset.
    pub fn enabled(&self) -> bool {
        self.service_account_token
            .as_deref()
            .is_some_and(|token| !token.trim().is_empty())
    }

    /// The registered service account token, trimmed, when it is non-blank.
    /// `None` when no usable token is set. The single read path used by the
    /// composition root to authenticate `op`.
    pub fn token(&self) -> Option<&str> {
        self.service_account_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
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

    /// Resolve a user-typed agent name to its variant, accepting the launch
    /// [`command`](Self::command) (`claude` / `codex` / `codex-fugu` / `gemini`),
    /// the [`display_name`](Self::display_name) (`sakana.ai` for codex-fugu), and
    /// the on-disk serde label (`codex_fugu`) that `usagi config` prints — all
    /// case-insensitively. Used by the 在席 prompt's `agent <name>` and
    /// `clean --agent`. Returns `None` for an unrecognised name.
    ///
    /// `-` and `_` are treated as the same separator so the serde label resolves:
    /// `codex_fugu` (what `config` shows) and `codex-fugu` (the launch command)
    /// differ only there, and a user copying the displayed name would otherwise
    /// hit "unknown agent CLI".
    pub fn from_name(name: &str) -> Option<AgentCli> {
        let normalize = |s: &str| s.trim().to_ascii_lowercase().replace('_', "-");
        let name = normalize(name);
        AgentCli::ALL
            .into_iter()
            .find(|cli| normalize(cli.command()) == name || normalize(cli.display_name()) == name)
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

/// A toggleable group of the Claude Code skills usagi ships to its agents.
///
/// usagi embeds a set of skills in its binary and symlinks them into every
/// session worktree (see [`crate::infrastructure::skills`]). The mandatory
/// `usagi-session` skill belongs to no feature and is always linked; every other
/// shipped skill is grouped under one of these features so the user can turn the
/// whole group on or off — globally in [`Settings`] and per-project in
/// [`LocalSettings`]. Adding a new toggleable skill means adding (or reusing) a
/// variant here and tagging the skill with it in `infrastructure::skills`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SkillFeature {
    /// The PR-workflow skills: creating, updating, and fixing a pull request
    /// (`usagi-pr-create` / `usagi-pr-update` / `usagi-pr-fix`).
    PullRequest,
}

impl SkillFeature {
    /// Every toggleable skill feature, in the order the config screen lists them.
    pub const ALL: [SkillFeature; 1] = [SkillFeature::PullRequest];

    /// The stable on-disk identifier used as the [`Settings::skill_features`] /
    /// [`LocalSettings::skill_features`] map key. Never change an existing id: it
    /// is what a saved `settings.json` keys the toggle under.
    pub fn id(self) -> &'static str {
        match self {
            SkillFeature::PullRequest => "pull-request",
        }
    }

    /// The human-facing label shown for this feature on the config screen.
    pub fn label(self) -> &'static str {
        match self {
            SkillFeature::PullRequest => "PR Skills",
        }
    }

    /// Whether the feature is enabled when neither the global settings nor a
    /// project override say otherwise. Shipped skills are opt-out: on by default.
    pub fn default_enabled(self) -> bool {
        match self {
            SkillFeature::PullRequest => true,
        }
    }

    /// Resolve a stored [`id`](Self::id) back to its feature, or `None` for an
    /// id no current usagi knows (e.g. a feature removed since the file was
    /// written, or a hand-edited typo).
    pub fn from_id(id: &str) -> Option<SkillFeature> {
        SkillFeature::ALL.into_iter().find(|f| f.id() == id)
    }
}

/// How many lines of scrolled-off output each embedded terminal pane keeps by
/// default, so the user can scroll a pane back over earlier output.
///
/// Every live pane holds its own scrollback buffer, so this cap is paid once per
/// pane: with many sessions and panes open at once it is the dominant slice of
/// the TUI's memory. The pool keeps every pane of every session alive in the
/// background, and each pane's parser holds up to this many scrollback rows of
/// `cols` cells, so worst-case resident grid memory is on the order of
/// `panes × terminal_scrollback_lines × cols × sizeof(cell)` — bounded (no
/// leak), but it scales with the number of open panes. The default is
/// deliberately modest — enough to scroll back over a command's recent output
/// without each pane reserving a large buffer — and the user can raise it (up to
/// [`MAX_TERMINAL_SCROLLBACK_LINES`]) when they want deeper history.
pub const DEFAULT_TERMINAL_SCROLLBACK_LINES: usize = 2_000;

/// The largest [`Settings::terminal_scrollback_lines`] value honoured. A
/// hand-edited `settings.json` could otherwise ask every pane to retain an
/// unbounded buffer; [`Settings::sanitized`] clamps to this so one setting
/// cannot blow up memory across every open pane.
pub const MAX_TERMINAL_SCROLLBACK_LINES: usize = 50_000;

/// The default used for a missing `terminal_scrollback_lines` field, so an older
/// `settings.json` (written before the field existed) loads with the modest
/// default rather than `0` — `#[serde(default)]` on the struct would otherwise
/// fall back to `usize`'s `0`, leaving panes with no scrollback at all.
fn default_terminal_scrollback_lines() -> usize {
    DEFAULT_TERMINAL_SCROLLBACK_LINES
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
    /// How the embedded terminal (没入) reserves its navigation keys — a `Ctrl-O`
    /// prefix or single `Alt`-chords — so the rest reach the shell / agent.
    #[serde(deserialize_with = "crate::domain::serde_fallback::or_default")]
    pub key_scheme: KeyScheme,
    /// Which state the home screen's left session sidebar opens in (`Ctrl-B`
    /// toggles it at runtime).
    #[serde(deserialize_with = "crate::domain::serde_fallback::or_default")]
    pub sidebar: Sidebar,
    /// Whether the sidebar mascot reacts to interaction — a quick blink in 切替 /
    /// 在席 and the 没入 working rabbit's idle paw motion. Purely cosmetic and
    /// driven by paints that already happen (no idle timer), so turning it off
    /// just keeps the mascot perfectly still. On unless the user disables it.
    pub mascot_animation_enabled: bool,
    /// How many lines of scrolled-off output each embedded terminal pane keeps.
    /// Paid once per live pane, so it is the main lever on the TUI's memory when
    /// many sessions are open; [`sanitized`](Self::sanitized) clamps it to
    /// [`MAX_TERMINAL_SCROLLBACK_LINES`].
    #[serde(default = "default_terminal_scrollback_lines")]
    pub terminal_scrollback_lines: usize,
    /// The optional local LLM the agent can offload light work to.
    pub local_llm: LocalLlm,
    /// The optional 1Password MCP server the agent can read secrets through.
    pub op_mcp: OpMcp,
    /// Which of usagi's optional shipped-skill features are enabled, keyed by
    /// [`SkillFeature::id`]. A feature absent from the map uses its
    /// [`default_enabled`](SkillFeature::default_enabled), so the map only ever
    /// records a value that differs from the default; query it through
    /// [`skill_feature_enabled`](Self::skill_feature_enabled) rather than reading
    /// the map directly. Unknown keys (a feature this usagi does not know) are
    /// dropped by [`sanitized`](Self::sanitized).
    #[serde(default)]
    pub skill_features: BTreeMap<String, bool>,
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
            key_scheme: KeyScheme::default(),
            sidebar: Sidebar::default(),
            // The mascot's reactions are opt-out: on unless the user disables them.
            mascot_animation_enabled: true,
            terminal_scrollback_lines: DEFAULT_TERMINAL_SCROLLBACK_LINES,
            local_llm: LocalLlm::default(),
            op_mcp: OpMcp::default(),
            // No overrides recorded: every shipped-skill feature uses its own
            // default (see [`SkillFeature::default_enabled`]).
            skill_features: BTreeMap::new(),
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
        // A blank service-account token is equivalent to "not configured" and
        // should not wire the 1Password MCP server. A non-blank token is trimmed
        // so accidental surrounding whitespace from copy/paste is not persisted
        // or passed through to `op`.
        self.op_mcp.service_account_token = self.op_mcp.token().map(str::to_string);
        // A hand-edited `settings.json` could ask every pane to keep an enormous
        // (or effectively unbounded) scrollback buffer; cap it so one setting
        // cannot exhaust memory across every open pane.
        self.terminal_scrollback_lines = self
            .terminal_scrollback_lines
            .min(MAX_TERMINAL_SCROLLBACK_LINES);
        // Drop skill-feature keys this usagi does not recognise (a feature
        // removed since the file was written, or a hand-edited typo) so a stale
        // entry never lingers in the saved file or the `usagi config` output.
        self.skill_features
            .retain(|id, _| SkillFeature::from_id(id).is_some());
        self
    }

    /// Whether the shipped-skill `feature` is enabled, resolving an absent entry
    /// to the feature's [`default_enabled`](SkillFeature::default_enabled). This
    /// is the single read path for a feature's enablement — callers never index
    /// [`skill_features`](Self::skill_features) directly.
    pub fn skill_feature_enabled(&self, feature: SkillFeature) -> bool {
        self.skill_features
            .get(feature.id())
            .copied()
            .unwrap_or_else(|| feature.default_enabled())
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
        // Each project-local skill-feature override replaces the global entry for
        // that feature; features the project does not mention keep the global
        // value. The merged map is still read through `skill_feature_enabled`.
        for (id, enabled) in &local.skill_features {
            self.skill_features.insert(id.clone(), *enabled);
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
            op_mcp_enabled: self.op_mcp.enabled(),
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
    /// Per-project overrides of shipped-skill features, keyed by
    /// [`SkillFeature::id`]. A feature present here overrides the global setting
    /// for this project; a feature absent here defers to the global value. Read
    /// the override through [`skill_feature_override`](Self::skill_feature_override).
    #[serde(default)]
    pub skill_features: BTreeMap<String, bool>,
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
            && self.skill_features.is_empty()
    }

    /// This project's override for the shipped-skill `feature`: `Some(enabled)`
    /// when the project pins it on or off, or `None` to defer to the global
    /// setting. The single read path for a local override — callers never index
    /// [`skill_features`](Self::skill_features) directly.
    pub fn skill_feature_override(&self, feature: SkillFeature) -> Option<bool> {
        self.skill_features.get(feature.id()).copied()
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
        // The on-disk serde label (`codex_fugu`) that `usagi config` prints
        // resolves as well — `-`/`_` are the same separator — so copying what
        // `config` shows into `agent` / `clean --agent` works.
        assert_eq!(AgentCli::from_name("codex_fugu"), Some(AgentCli::CodexFugu));
        assert_eq!(
            AgentCli::from_name(" Codex_Fugu "),
            Some(AgentCli::CodexFugu)
        );
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
    fn agent_wiring_enables_op_mcp_only_when_a_token_is_registered() {
        // Disabled by default: no 1Password token registered.
        let mut settings = Settings::default();
        assert!(!settings.agent_wiring("usagi").op_mcp_enabled);
        // A blank token still counts as not registered.
        settings.op_mcp.service_account_token = Some("   ".to_string());
        assert!(!settings.agent_wiring("usagi").op_mcp_enabled);
        // A non-blank token turns the wiring on.
        settings.op_mcp.service_account_token = Some("ops_token".to_string());
        assert!(settings.agent_wiring("usagi").op_mcp_enabled);
    }

    #[test]
    fn op_mcp_enabled_and_token_treat_blank_as_unset() {
        // Default: no token, disabled, nothing to read.
        let default = OpMcp::default();
        assert!(!default.enabled());
        assert_eq!(default.token(), None);
        // Whitespace-only: treated as unset.
        let blank = OpMcp {
            service_account_token: Some("  \t ".to_string()),
        };
        assert!(!blank.enabled());
        assert_eq!(blank.token(), None);
        // A real token: enabled, and `token()` returns it trimmed.
        let set = OpMcp {
            service_account_token: Some("  ops_abc  ".to_string()),
        };
        assert!(set.enabled());
        assert_eq!(set.token(), Some("ops_abc"));
    }

    #[test]
    fn sanitized_trims_or_clears_the_op_mcp_token() {
        // Surrounding whitespace is trimmed off a real token.
        let padded = Settings {
            op_mcp: OpMcp {
                service_account_token: Some("  ops_abc  ".to_string()),
            },
            ..Default::default()
        };
        assert_eq!(
            padded.sanitized().op_mcp.service_account_token.as_deref(),
            Some("ops_abc")
        );
        // A blank token is cleared to None so it never wires the server.
        let blank = Settings {
            op_mcp: OpMcp {
                service_account_token: Some("   ".to_string()),
            },
            ..Default::default()
        };
        assert_eq!(blank.sanitized().op_mcp.service_account_token, None);
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
    fn terminal_scrollback_lines_defaults_to_the_modest_cap() {
        // The default is the modest per-pane cap, not `usize`'s `0` — every pane
        // keeps at least this much scrollback out of the box.
        assert_eq!(
            Settings::default().terminal_scrollback_lines,
            DEFAULT_TERMINAL_SCROLLBACK_LINES
        );
        assert_eq!(default_terminal_scrollback_lines(), 2_000);
    }

    #[test]
    fn terminal_scrollback_lines_falls_back_when_the_field_is_absent() {
        // An older `settings.json` written before the field existed must load with
        // the modest default rather than `0` (which would leave panes with no
        // scrollback). `{}` stands in for any file missing the key.
        let loaded: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(
            loaded.terminal_scrollback_lines,
            DEFAULT_TERMINAL_SCROLLBACK_LINES
        );
        // A stored value is taken verbatim (clamping is `sanitized`'s job).
        let stored: Settings =
            serde_json::from_str(r#"{"terminal_scrollback_lines": 500}"#).unwrap();
        assert_eq!(stored.terminal_scrollback_lines, 500);
    }

    #[test]
    fn sanitized_clamps_an_oversized_scrollback_but_leaves_a_sane_one() {
        // A value within the cap is preserved untouched.
        let sane = Settings {
            terminal_scrollback_lines: 1_000,
            ..Default::default()
        };
        assert_eq!(sane.sanitized().terminal_scrollback_lines, 1_000);

        // A value past the cap — e.g. a hand-edited file asking every pane to keep
        // an unbounded buffer — is clamped to the maximum.
        let huge = Settings {
            terminal_scrollback_lines: usize::MAX,
            ..Default::default()
        };
        assert_eq!(
            huge.sanitized().terminal_scrollback_lines,
            MAX_TERMINAL_SCROLLBACK_LINES
        );
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
    fn key_scheme_defaults_to_prefix_and_serializes_in_snake_case() {
        // 没入 opens with the Ctrl-O prefix scheme unless configured otherwise —
        // it claims only one chord and works on any terminal without setup.
        assert_eq!(KeyScheme::default(), KeyScheme::Prefix);
        assert_eq!(Settings::default().key_scheme, KeyScheme::Prefix);
        // ALL lists both schemes once, in cycle order (prefix first).
        assert_eq!(KeyScheme::ALL, [KeyScheme::Prefix, KeyScheme::Alt]);
        // Round-trips through the snake_case JSON the rest of the settings use.
        assert_eq!(
            serde_json::to_string(&KeyScheme::Prefix).unwrap(),
            "\"prefix\""
        );
        assert_eq!(serde_json::to_string(&KeyScheme::Alt).unwrap(), "\"alt\"");
        assert_eq!(
            serde_json::from_str::<KeyScheme>("\"alt\"").unwrap(),
            KeyScheme::Alt
        );
    }

    #[test]
    fn key_scheme_falls_back_to_default_when_absent_or_unknown() {
        // An older settings.json (written before the field existed) loads with the
        // default scheme rather than failing.
        let loaded: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(loaded.key_scheme, KeyScheme::Prefix);
        // A hand-edited unknown value degrades to the default too (serde_fallback).
        let bad: Settings = serde_json::from_str(r#"{"key_scheme": "chord"}"#).unwrap();
        assert_eq!(bad.key_scheme, KeyScheme::Prefix);
    }

    #[test]
    fn sidebar_toggles_between_full_and_rail() {
        assert_eq!(Sidebar::Full.toggled(), Sidebar::Rail);
        assert_eq!(Sidebar::Rail.toggled(), Sidebar::Full);
    }

    #[test]
    fn mascot_animation_defaults_on_and_round_trips() {
        // The mascot's reactions are opt-out: on unless explicitly disabled.
        assert!(Settings::default().mascot_animation_enabled);
        // An explicit `false` survives a JSON round-trip, and an absent field
        // falls back to the default (`true`) via `#[serde(default)]`.
        let off = Settings {
            mascot_animation_enabled: false,
            ..Default::default()
        };
        let json = serde_json::to_string(&off).unwrap();
        assert_eq!(serde_json::from_str::<Settings>(&json).unwrap(), off);
        assert!(
            serde_json::from_str::<Settings>("{}")
                .unwrap()
                .mascot_animation_enabled
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

    #[test]
    fn skill_feature_metadata_is_consistent() {
        // Every feature in ALL round-trips through its id and is on by default
        // (shipped skills are opt-out).
        for feature in SkillFeature::ALL {
            assert_eq!(SkillFeature::from_id(feature.id()), Some(feature));
            assert!(!feature.label().is_empty());
            assert!(feature.default_enabled());
        }
        assert_eq!(SkillFeature::PullRequest.id(), "pull-request");
        // An unknown id resolves to nothing.
        assert_eq!(SkillFeature::from_id("nope"), None);
    }

    #[test]
    fn skill_feature_enabled_defaults_on_and_honours_an_explicit_value() {
        // Absent from the map → the feature's default (on).
        let mut settings = Settings::default();
        assert!(settings.skill_feature_enabled(SkillFeature::PullRequest));
        // An explicit `false` is honoured...
        settings
            .skill_features
            .insert("pull-request".to_string(), false);
        assert!(!settings.skill_feature_enabled(SkillFeature::PullRequest));
        // ...as is an explicit `true`.
        settings
            .skill_features
            .insert("pull-request".to_string(), true);
        assert!(settings.skill_feature_enabled(SkillFeature::PullRequest));
    }

    #[test]
    fn sanitized_drops_unknown_skill_feature_keys() {
        let mut settings = Settings::default();
        settings
            .skill_features
            .insert("pull-request".to_string(), false);
        // A key no current usagi knows (a removed feature or a hand-edited typo).
        settings
            .skill_features
            .insert("ghost-feature".to_string(), true);
        let cleaned = settings.sanitized();
        // The known key survives; the unknown one is dropped.
        assert_eq!(
            cleaned.skill_features.get("pull-request").copied(),
            Some(false)
        );
        assert!(!cleaned.skill_features.contains_key("ghost-feature"));
    }

    #[test]
    fn with_local_overrides_a_skill_feature_when_set() {
        // Global leaves the PR skills on (the default); a project override turns
        // them off for just this project.
        let global = Settings::default();
        assert!(global.skill_feature_enabled(SkillFeature::PullRequest));
        let mut local = LocalSettings::default();
        local
            .skill_features
            .insert("pull-request".to_string(), false);

        let effective = global.with_local(&local);
        assert!(!effective.skill_feature_enabled(SkillFeature::PullRequest));
    }

    #[test]
    fn is_empty_counts_a_skill_feature_override() {
        assert!(LocalSettings::default().is_empty());
        let mut local = LocalSettings::default();
        local
            .skill_features
            .insert("pull-request".to_string(), false);
        assert!(!local.is_empty());
        assert_eq!(
            local.skill_feature_override(SkillFeature::PullRequest),
            Some(false)
        );
        // A feature the project does not mention defers to the global setting.
        assert_eq!(
            LocalSettings::default().skill_feature_override(SkillFeature::PullRequest),
            None
        );
    }

    #[test]
    fn skill_features_default_to_empty_and_round_trip() {
        // An older settings.json (written before the field existed) loads with an
        // empty map — every feature then uses its default.
        let loaded: Settings = serde_json::from_str("{}").unwrap();
        assert!(loaded.skill_features.is_empty());
        assert!(loaded.skill_feature_enabled(SkillFeature::PullRequest));
        // An explicit entry survives a JSON round-trip.
        let mut settings = Settings::default();
        settings
            .skill_features
            .insert("pull-request".to_string(), false);
        let json = serde_json::to_string(&settings).unwrap();
        assert_eq!(serde_json::from_str::<Settings>(&json).unwrap(), settings);
    }
}

//! Application settings.
//!
//! The global, per-user preferences persisted as `settings.json` in the data
//! directory, plus workspace settings persisted beside a project. Theme, icon
//! rendering, modal interaction, and the generic Terminal PTY ceiling stay global; Agent,
//! Workflow, Team, Issue, and Memory values are copied to a workspace when it is
//! registered and may then be changed independently.
//! Environment bindings ([`env`]) exist in both scopes and merge, so a workspace
//! adds to — or overrides — what every workspace inherits.
//!
//! Enum-valued settings degrade an unrecognised stored token to a sensible
//! default rather than failing the whole file, so a value written by a newer
//! usagi — or a hand-edited typo — never blocks loading. [`Theme`] does this with
//! `#[serde(other)]` on [`Theme::System`] (unknown → follow the OS).

mod env;

pub use env::{
    EnvBindings, EnvLimitError, MAX_CONCURRENT_SECRET_READS, MAX_ENV_BINDINGS,
    MAX_SECRET_REFERENCES, SECRET_REFERENCE_PREFIX, format_env_bindings, is_secret_reference,
    is_valid_env_name, parse_env_bindings, valid_bindings, validate_env_limits,
};

use serde::{Deserialize, Serialize};

/// Default number of daemon-owned generic Terminal PTYs admitted at once.
pub const DEFAULT_TERMINAL_MAX_CONCURRENT: u16 = 64;
/// Hard ceiling accepted from settings. This bounds PTYs and their file
/// descriptors even when screen and journal usage remain small.
pub const TERMINAL_MAX_CONCURRENT_MAX: u16 = 256;
/// Values offered by the Config screen. Hand-edited settings may use any value
/// in the supported range.
pub const TERMINAL_CONCURRENCY_PRESETS: [u16; 5] = [16, 32, 64, 128, 256];

/// Global safety ceiling for concurrently owned generic Terminal PTYs.
///
/// This is deliberately not the primary memory budget: terminal screens and
/// journals are bounded from their actual retained usage. It remains as a last
/// line of defence for OS PTYs, processes, threads, and file descriptors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct TerminalConcurrencyLimit(u16);

impl TerminalConcurrencyLimit {
    /// Build a supported limit.
    #[must_use]
    pub const fn new(value: u16) -> Option<Self> {
        if value >= 1 && value <= TERMINAL_MAX_CONCURRENT_MAX {
            Some(Self(value))
        } else {
            None
        }
    }

    /// Return the configured PTY ceiling.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0 as usize
    }

    /// Move through the Config screen's bounded presets.
    #[must_use]
    pub fn cycle(self, forward: bool) -> Self {
        let current = self.0;
        let next = if forward {
            TERMINAL_CONCURRENCY_PRESETS
                .into_iter()
                .find(|candidate| *candidate > current)
                .unwrap_or(TERMINAL_CONCURRENCY_PRESETS[0])
        } else {
            TERMINAL_CONCURRENCY_PRESETS
                .into_iter()
                .rev()
                .find(|candidate| *candidate < current)
                .unwrap_or(TERMINAL_MAX_CONCURRENT_MAX)
        };
        Self(next)
    }
}

impl Default for TerminalConcurrencyLimit {
    fn default() -> Self {
        Self(DEFAULT_TERMINAL_MAX_CONCURRENT)
    }
}

impl<'de> Deserialize<'de> for TerminalConcurrencyLimit {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = u16::deserialize(deserializer)?;
        Self::new(value).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "terminal_max_concurrent must be between 1 and {TERMINAL_MAX_CONCURRENT_MAX}"
            ))
        })
    }
}

/// UI color theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    /// A light palette.
    Light,
    /// A dark palette.
    Dark,
    /// Follow the OS appearance. The default, and the state an unrecognised
    /// stored token degrades to — so it must stay the last variant for
    /// `#[serde(other)]`.
    #[default]
    #[serde(other)]
    System,
}

/// Whether terminal chrome uses Nerd Font glyphs or readable text labels.
///
/// A terminal cannot report whether its configured font contains private-use
/// glyphs, so this is an explicit global preference. Nerd Font is the default
/// visual language; [`Self::Text`] is the deterministic fallback for terminals
/// without a patched font.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IconMode {
    /// Use self-explanatory ASCII labels and markers.
    Text,
    /// Use Nerd Font glyphs for compact terminal chrome. This is also the
    /// fallback for unknown stored tokens.
    #[default]
    #[serde(other)]
    NerdFont,
}

/// How Overview and Closeup accept a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModalSelectionMode {
    /// Type the command into a prompt.
    Prompt,
    /// Choose a command from the visible action list. The default, and the
    /// state an unrecognised stored token degrades to.
    #[default]
    #[serde(other)]
    Action,
}

/// How a newly detected PR may interrupt Home.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrAutoOpen {
    /// Open from Switch and Closeup when no other surface owns input.
    Always,
    /// Open only from Switch. This keeps live terminal input stable by default.
    #[default]
    SwitchOnly,
    /// Keep the modal closed and show a quiet notice.
    NotifyOnly,
    /// Update only the sidebar badge.
    #[serde(other)]
    Never,
}

/// Built-in Agent team structure selected from Config.
///
/// `None` preserves the role-less compatibility mode. The other values select
/// a code-defined role catalog; workspace `roles.toml` may layer over that
/// catalog without changing this stable selection token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamTemplate {
    /// Director → Manager → Worker.
    Hierarchical,
    /// Director directly coordinates parallel Workers.
    Flat,
    /// Planner → Implementer → Tester staged delivery.
    Pipeline,
    /// Do not inject a built-in role catalog. The default, and the state an
    /// unrecognised stored token degrades to.
    #[default]
    #[serde(other)]
    None,
}

/// The workspace interaction model used when starting Director work.
///
/// `Classic` preserves the existing conversation-first flow. `GoalDriven`
/// opens a goal composer and starts the Director with an autonomous delivery
/// contract. The compatibility mode is deliberately the serde fallback so an
/// older settings file, a future token, or an omitted field cannot opt a user
/// into autonomous work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkMode {
    /// One goal starts a Director-owned run through PR readiness or an explicit
    /// human decision.
    GoalDriven,
    /// Existing session- and conversation-first interaction. This must remain
    /// last because it is also serde's unknown-token fallback.
    #[default]
    #[serde(other)]
    Classic,
}

impl WorkMode {
    /// Toggle between compatibility and goal-driven interaction.
    #[must_use]
    pub const fn cycle(self) -> Self {
        match self {
            Self::Classic => Self::GoalDriven,
            Self::GoalDriven => Self::Classic,
        }
    }
}

impl TeamTemplate {
    /// Every selectable Config value in display order.
    pub const ALL: [Self; 4] = [Self::None, Self::Hierarchical, Self::Flat, Self::Pipeline];

    /// Select the adjacent value, wrapping at either edge.
    #[must_use]
    pub fn cycle(self, forward: bool) -> Self {
        let index = Self::ALL
            .iter()
            .position(|candidate| *candidate == self)
            .unwrap_or_default();
        let next = if forward {
            (index + 1) % Self::ALL.len()
        } else {
            (index + Self::ALL.len() - 1) % Self::ALL.len()
        };
        Self::ALL[next]
    }
}

/// The public, non-secret status invocation that decides whether an agent CLI is
/// ready to launch.
///
/// It travels with the rest of the agent CLI vocabulary
/// ([`DefaultModel::readiness_command`]) so a provider cannot be added to the
/// picker without also declaring how a launcher proves that CLI is usable. The
/// arguments are literal, product-documented status subcommands: they carry no
/// credential, configuration path, or user input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentReadinessCommand {
    program: &'static str,
    arguments: &'static [&'static str],
}

impl AgentReadinessCommand {
    /// The executable to run, identical to [`DefaultModel::command`].
    #[must_use]
    pub const fn program(self) -> &'static str {
        self.program
    }

    /// The status subcommand arguments passed to [`program`](Self::program).
    #[must_use]
    pub const fn arguments(self) -> &'static [&'static str] {
        self.arguments
    }
}

/// The cloud model provider used when a new Agent pane has no explicit profile.
///
/// This is also the closed vocabulary a user selects from with the Closeup
/// `agent -m <model>` command, so [`selector`](Self::selector),
/// [`profile_id`](Self::profile_id), [`command`](Self::command), and
/// [`readiness_command`](Self::readiness_command) are the single source of truth
/// for the typed token, the daemon profile it launches, the executable whose
/// presence makes it usable, and the status probe that proves it usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefaultModel {
    /// Anthropic Claude, launched through the `claude` profile.
    Claude,
    /// Sakana AI's Codex-compatible CLI. Presented as `sakana.ai` and launched
    /// through the `sakana-ai` profile, whose executable is `codex-fugu`.
    #[serde(alias = "codex_fugu", alias = "sakana.ai")]
    SakanaAi,
    /// `OpenAI`, launched through the Codex `codex` profile.
    #[default]
    #[serde(rename = "openai", other)]
    OpenAi,
}

impl DefaultModel {
    /// Every selectable model provider, in the order menus and completion list
    /// them.
    pub const ALL: [Self; 3] = [Self::Claude, Self::OpenAi, Self::SakanaAi];

    /// Stable daemon profile ID selected by this model provider.
    #[must_use]
    pub const fn profile_id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::OpenAi => "codex",
            Self::SakanaAi => "sakana-ai",
        }
    }

    /// The executable whose availability decides whether this provider can be
    /// selected. It is deliberately distinct from
    /// [`profile_id`](Self::profile_id): `sakana-ai` runs `codex-fugu`.
    #[must_use]
    pub const fn command(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::OpenAi => "codex",
            Self::SakanaAi => "codex-fugu",
        }
    }

    /// The `$HOME`-relative directory this provider's CLI writes its own state
    /// and auth cache into (`~/.claude`, `~/.codex`, `~/.codex-fugu`).
    ///
    /// It belongs next to [`command`](Self::command) because the executable and
    /// the directory it writes are one fact: a launcher that confines writes has
    /// to grant the state directory of the CLI it actually spawns, and a renamed
    /// executable must not leave that grant pointing at another provider's
    /// state. `sakana-ai` runs `codex-fugu`, whose state is `~/.codex-fugu`, so
    /// it never shares Codex's rollouts.
    #[must_use]
    pub const fn state_directory(self) -> &'static str {
        match self {
            Self::Claude => ".claude",
            Self::OpenAi => ".codex",
            Self::SakanaAi => ".codex-fugu",
        }
    }

    /// The `$HOME`-relative path prefix of the global config this provider's CLI
    /// writes next to its state directory, when it keeps that config outside the
    /// directory itself (Claude writes `~/.claude.json`). Codex and `codex-fugu`
    /// keep their config inside [`state_directory`](Self::state_directory), so
    /// they have no separate prefix.
    ///
    /// It is a **prefix**, not one file: Claude saves the config by writing
    /// `~/.claude.json.tmp.<pid>.<random>` under the `~/.claude.json.lock` lock and
    /// renaming it over `~/.claude.json`, and keeps `~/.claude.json.backup.<ms>`
    /// snapshots beside it. A launcher that grants only the exact file leaves every
    /// save failing, so onboarding, folder trust and MCP approvals never persist.
    #[must_use]
    pub const fn global_config_prefix(self) -> Option<&'static str> {
        match self {
            Self::Claude => Some(".claude.json"),
            Self::OpenAi | Self::SakanaAi => None,
        }
    }

    /// The user-facing token typed after `agent -m` and shown in the picker.
    #[must_use]
    pub const fn selector(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::OpenAi => "codex",
            Self::SakanaAi => "sakana.ai",
        }
    }

    /// The non-secret status probe a launcher runs before spawning this
    /// provider's CLI.
    ///
    /// Codex and the Codex-compatible `codex-fugu` share the same CLI grammar,
    /// so both prove readiness with `login status`; Claude uses `auth status`.
    /// The probe deliberately reuses [`command`](Self::command) rather than
    /// naming an executable again, so a renamed executable cannot leave the
    /// probe pointing at the old one.
    #[must_use]
    pub const fn readiness_command(self) -> AgentReadinessCommand {
        AgentReadinessCommand {
            program: self.command(),
            arguments: match self {
                Self::Claude => &["auth", "status"],
                Self::OpenAi | Self::SakanaAi => &["login", "status"],
            },
        }
    }

    /// The single decision a launcher makes about readiness: resolve a product
    /// token to the status probe that proves that CLI usable, or refuse.
    ///
    /// The token is resolved with [`from_selector`](Self::from_selector), so an
    /// executable (`codex-fugu`), a profile ID (`sakana-ai`), and a selector
    /// (`sakana.ai`) all reach the same probe. An unknown token yields `None`,
    /// which keeps a launcher fail-closed on a product it does not model.
    #[must_use]
    pub fn readiness_command_for(token: &str) -> Option<AgentReadinessCommand> {
        Self::from_selector(token).map(Self::readiness_command)
    }

    /// Resolve a user-typed token to its provider, accepting the
    /// [`selector`](Self::selector), the [`profile_id`](Self::profile_id), and
    /// the [`command`](Self::command) case-insensitively. `-`, `_`, and `.` are
    /// treated as the same separator, so `sakana_ai` and `sakana.ai` both
    /// resolve.
    #[must_use]
    pub fn from_selector(token: &str) -> Option<Self> {
        let normalize = |value: &str| value.trim().to_ascii_lowercase().replace(['_', '.'], "-");
        let token = normalize(token);
        (!token.is_empty()).then_some(())?;
        Self::ALL.into_iter().find(|model| {
            [model.selector(), model.profile_id(), model.command()]
                .into_iter()
                .any(|candidate| normalize(candidate) == token)
        })
    }
}

/// The model providers whose CLI is installed on this machine.
///
/// Availability is observed by the composition root as one PATH lookup snapshot
/// (without executing provider CLIs) and injected, so every surface that offers
/// a provider — the Config screen and the Closeup `agent -m` picker and
/// completion — offers exactly the providers that can actually run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AvailableModels {
    claude: bool,
    open_ai: bool,
    sakana_ai: bool,
}

impl AvailableModels {
    /// Construct availability from an observed set of providers.
    #[must_use]
    pub fn new(models: impl IntoIterator<Item = DefaultModel>) -> Self {
        let mut available = Self::default();
        for model in models {
            match model {
                DefaultModel::Claude => available.claude = true,
                DefaultModel::OpenAi => available.open_ai = true,
                DefaultModel::SakanaAi => available.sakana_ai = true,
            }
        }
        available
    }

    /// Availability used by callers that do not supply a system probe.
    #[must_use]
    pub fn all() -> Self {
        Self::new(DefaultModel::ALL)
    }

    /// Whether no provider is installed.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        !self.claude && !self.open_ai && !self.sakana_ai
    }

    /// Whether this exact provider can be selected.
    #[must_use]
    pub const fn contains(self, model: DefaultModel) -> bool {
        match model {
            DefaultModel::Claude => self.claude,
            DefaultModel::OpenAi => self.open_ai,
            DefaultModel::SakanaAi => self.sakana_ai,
        }
    }

    /// Every selectable provider in [`DefaultModel::ALL`] order.
    pub fn iter(self) -> impl Iterator<Item = DefaultModel> {
        DefaultModel::ALL
            .into_iter()
            .filter(move |model| self.contains(*model))
    }

    /// The first selectable provider, used when a stored choice is not
    /// installed.
    #[must_use]
    pub fn first(self) -> Option<DefaultModel> {
        // `OpenAi` is the stored default, so it stays the first offer whenever
        // it is installed.
        [
            DefaultModel::OpenAi,
            DefaultModel::Claude,
            DefaultModel::SakanaAi,
        ]
        .into_iter()
        .find(|model| self.contains(*model))
    }

    /// The next selectable provider after `model`, wrapping in
    /// [`DefaultModel::ALL`] order.
    #[must_use]
    pub fn next(self, model: DefaultModel) -> Option<DefaultModel> {
        let ordered: Vec<_> = self.iter().collect();
        let position = ordered.iter().position(|candidate| *candidate == model);
        match position {
            Some(index) => ordered.get((index + 1) % ordered.len()).copied(),
            None => self.first(),
        }
    }
}

/// The global, per-user application settings.
///
/// A missing field (and the whole file) falls back to [`Default`], and each enum
/// field degrades an unrecognised token to its default, so an older or
/// hand-edited `settings.json` still loads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The UI color theme.
    pub theme: Theme,
    /// Nerd Font glyphs or text fallbacks for terminal chrome.
    pub icon_mode: IconMode,
    /// The command-selection interaction used by Overview and Closeup modals.
    pub modal_selection_mode: ModalSelectionMode,
    /// Whether a newly detected PR opens its modal automatically.
    pub pr_auto_open: PrAutoOpen,
    /// Global generic Terminal PTY safety ceiling. Actual screen and journal
    /// memory are bounded independently by their retained usage.
    pub terminal_max_concurrent: TerminalConcurrencyLimit,
    /// The provider used for Agent panes when no profile is selected explicitly.
    pub default_model: DefaultModel,
    /// Fully-qualified Git ref selected by default when creating a session.
    /// `None` follows the workspace's current checkout.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    /// Whether issue-backed MCP tools are available to agents.
    pub issue_enabled: bool,
    /// Whether durable-memory MCP tools are available to agents.
    pub memory_enabled: bool,
    /// Built-in role catalog used for new and resumed Agent work.
    pub team_template: TeamTemplate,
    /// Whether Director starts as a classic conversation or from one goal.
    pub work_mode: WorkMode,
    /// Environment bindings injected into every workspace's Agent and terminal
    /// children. The key is the variable name, the value a literal or a
    /// `op://…` secret reference; a workspace adds to or overrides them through
    /// [`LocalSettings::env`]. Read the usable ones with [`Self::env_bindings`].
    pub env: EnvBindings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: Theme::default(),
            icon_mode: IconMode::default(),
            modal_selection_mode: ModalSelectionMode::default(),
            pr_auto_open: PrAutoOpen::default(),
            terminal_max_concurrent: TerminalConcurrencyLimit::default(),
            default_model: DefaultModel::default(),
            default_branch: None,
            issue_enabled: true,
            memory_enabled: true,
            team_template: TeamTemplate::default(),
            work_mode: WorkMode::default(),
            // No environment is injected unless it is configured explicitly.
            env: EnvBindings::new(),
        }
    }
}

impl Settings {
    /// Replace the fields owned by the Config surface, keeping settings owned
    /// by dedicated editors and runtime integrations.
    ///
    /// Config saves merge their draft into the latest persisted document. This
    /// prevents a Config modal opened earlier from rolling back a concurrent
    /// environment or local-LLM update. If another Config writer changed the
    /// same owned field, the writer that saves last deliberately wins.
    #[must_use]
    pub fn with_config(mut self, settings: &Self) -> Self {
        self.theme = settings.theme;
        self.icon_mode = settings.icon_mode;
        self.modal_selection_mode = settings.modal_selection_mode;
        self.pr_auto_open = settings.pr_auto_open;
        self.terminal_max_concurrent = settings.terminal_max_concurrent;
        self.default_model = settings.default_model;
        self.issue_enabled = settings.issue_enabled;
        self.memory_enabled = settings.memory_enabled;
        self.team_template = settings.team_template;
        self.work_mode = settings.work_mode;
        self
    }

    /// Apply workspace-owned Agent, Base branch, Workflow, Team, Issue, Memory, and
    /// environment values over this global baseline. Theme, icon rendering, modal interaction,
    /// and the generic Terminal PTY ceiling always remain global.
    ///
    /// Environment bindings accumulate rather than replace: the workspace map is
    /// layered on top of the global one, so a same-named binding takes the
    /// workspace value and everything else stays inherited.
    #[must_use]
    pub fn with_local(mut self, local: &LocalSettings) -> Self {
        if let Some(model) = local.default_model {
            self.default_model = model;
        }
        if let Some(branch) = &local.default_branch {
            self.default_branch = Some(branch.clone());
        }
        if let Some(enabled) = local.issue_enabled {
            self.issue_enabled = enabled;
        }
        if let Some(enabled) = local.memory_enabled {
            self.memory_enabled = enabled;
        }
        if let Some(template) = local.team_template {
            self.team_template = template;
        }
        if let Some(mode) = local.work_mode {
            self.work_mode = mode;
        }
        for (name, value) in valid_bindings(&local.env) {
            self.env.insert(name.to_owned(), value.to_owned());
        }
        self
    }

    /// The bindings usable for injection, with invalid names and blank values
    /// dropped.
    pub fn env_bindings(&self) -> impl Iterator<Item = (&str, &str)> {
        valid_bindings(&self.env)
    }
}

/// Per-workspace Agent, Base branch, Workflow, Team, Issue, and Memory settings stored in
/// `<workspace>/.usagi/settings.json` (or the development-mode-specific `dev`
/// directory).
///
/// These values are initialized from the global workspace defaults when a
/// workspace is registered. An absent or unrecognised field temporarily defers
/// to the global value, which keeps older and hand-edited files safe to load.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalSettings {
    #[serde(deserialize_with = "deserialize_local_default_model")]
    pub default_model: Option<DefaultModel>,
    /// Workspace-specific session base ref. Absence follows the current checkout.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    pub issue_enabled: Option<bool>,
    pub memory_enabled: Option<bool>,
    /// Workspace override for the built-in team template.
    #[serde(deserialize_with = "deserialize_local_team_template")]
    pub team_template: Option<TeamTemplate>,
    /// Workspace override for the Director interaction model.
    #[serde(deserialize_with = "deserialize_local_work_mode")]
    pub work_mode: Option<WorkMode>,
    /// Environment bindings this workspace adds to the global ones. An empty map
    /// means the workspace uses exactly what it inherits.
    pub env: EnvBindings,
}

impl LocalSettings {
    /// Replace the Agent, Base branch, Workflow, Team, Issue, and Memory choices with
    /// `settings`, keeping this workspace's own environment bindings.
    ///
    /// The Config surface edits a merged [`Settings`] view, which carries the
    /// *inherited* environment; writing that view back verbatim would copy every
    /// global binding into the workspace file. Config saves therefore go through
    /// here so the workspace keeps owning only what it declared itself.
    #[must_use]
    pub fn with_config(mut self, settings: &Settings) -> Self {
        self.default_model = Some(settings.default_model);
        self.default_branch.clone_from(&settings.default_branch);
        self.issue_enabled = Some(settings.issue_enabled);
        self.memory_enabled = Some(settings.memory_enabled);
        self.team_template = Some(settings.team_template);
        self.work_mode = Some(settings.work_mode);
        self
    }

    /// The bindings usable for injection, with invalid names and blank values
    /// dropped.
    pub fn env_bindings(&self) -> impl Iterator<Item = (&str, &str)> {
        valid_bindings(&self.env)
    }
}

impl From<&Settings> for LocalSettings {
    fn from(settings: &Settings) -> Self {
        Self::default().with_config(settings)
    }
}

fn deserialize_local_default_model<'de, D>(
    deserializer: D,
) -> Result<Option<DefaultModel>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let token = Option::<String>::deserialize(deserializer)?;
    Ok(match token.as_deref() {
        Some("claude") => Some(DefaultModel::Claude),
        Some("openai") => Some(DefaultModel::OpenAi),
        Some("sakana_ai" | "sakana.ai" | "codex_fugu") => Some(DefaultModel::SakanaAi),
        _ => None,
    })
}

fn deserialize_local_team_template<'de, D>(
    deserializer: D,
) -> Result<Option<TeamTemplate>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let token = Option::<String>::deserialize(deserializer)?;
    Ok(match token.as_deref() {
        Some("hierarchical") => Some(TeamTemplate::Hierarchical),
        Some("flat") => Some(TeamTemplate::Flat),
        Some("pipeline") => Some(TeamTemplate::Pipeline),
        // `none` and unknown future values both disable delegation instead of
        // inheriting a potentially more permissive global template.
        Some(_) => Some(TeamTemplate::None),
        None => None,
    })
}

fn deserialize_local_work_mode<'de, D>(deserializer: D) -> Result<Option<WorkMode>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let token = Option::<String>::deserialize(deserializer)?;
    Ok(match token.as_deref() {
        Some("goal_driven") => Some(WorkMode::GoalDriven),
        // An explicit unknown token stays fail-closed in compatibility mode;
        // only an absent field inherits the global workspace default.
        Some(_) => Some(WorkMode::Classic),
        None => None,
    })
}

#[cfg(test)]
mod tests;

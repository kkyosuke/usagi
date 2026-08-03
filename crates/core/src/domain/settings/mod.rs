//! Application settings.
//!
//! The global, per-user preferences persisted as `settings.json` in the data
//! directory, plus workspace settings persisted beside a project. Theme and
//! modal interaction stay global; Agent, Issue, and Memory values are copied to
//! a workspace when it is registered and may then be changed independently.
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

/// Local models that may be exposed through the optional `usagi-llm` MCP
/// server.
///
/// This closed vocabulary is the trust boundary for the model token eventually
/// passed to `usagi llm-mcp --model`. A hand-edited settings file cannot add a
/// new argv/config token.
pub const LOCAL_LLM_MODELS: [&str; 4] = [
    "qwen2.5-coder:7b",
    "qwen2.5-coder:3b",
    "qwen2.5-coder:1.5b",
    "qwen2.5:7b",
];

/// The model used when the stored local-LLM model is absent or untrusted.
pub const DEFAULT_LOCAL_LLM_MODEL: &str = LOCAL_LLM_MODELS[0];

/// Trusted configuration for the optional local-LLM MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalLlm {
    /// Whether Claude and Codex launches receive the `usagi-llm` MCP server.
    pub enabled: bool,
    /// The allowlisted model served by `usagi llm-mcp`.
    pub model: String,
}

impl Default for LocalLlm {
    fn default() -> Self {
        Self {
            enabled: false,
            model: DEFAULT_LOCAL_LLM_MODEL.to_owned(),
        }
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
/// Availability is observed by the composition root (a PATH / `--version`
/// probe) and injected, so every surface that offers a provider — the Config
/// screen and the Closeup `agent -m` picker and completion — offers exactly the
/// providers that can actually run.
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
    /// The command-selection interaction used by Overview and Closeup modals.
    pub modal_selection_mode: ModalSelectionMode,
    /// The provider used for Agent panes when no profile is selected explicitly.
    pub default_model: DefaultModel,
    /// Whether issue-backed MCP tools are available to agents.
    pub issue_enabled: bool,
    /// Whether durable-memory MCP tools are available to agents.
    pub memory_enabled: bool,
    /// Optional local LLM exposed only through daemon-owned Agent provisioning.
    pub local_llm: LocalLlm,
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
            modal_selection_mode: ModalSelectionMode::default(),
            default_model: DefaultModel::default(),
            issue_enabled: true,
            memory_enabled: true,
            local_llm: LocalLlm::default(),
            // No environment is injected unless it is configured explicitly.
            env: EnvBindings::new(),
        }
    }
}

impl Settings {
    /// Coerce values loaded from the editable settings file into their trusted
    /// vocabulary before they can reach daemon-owned process configuration.
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        if !LOCAL_LLM_MODELS.contains(&self.local_llm.model.as_str()) {
            DEFAULT_LOCAL_LLM_MODEL.clone_into(&mut self.local_llm.model);
        }
        self
    }

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
        self.modal_selection_mode = settings.modal_selection_mode;
        self.default_model = settings.default_model;
        self.issue_enabled = settings.issue_enabled;
        self.memory_enabled = settings.memory_enabled;
        self
    }

    /// Apply workspace-owned Agent, Issue, Memory, and environment values over
    /// this global baseline. Theme and modal interaction always remain global.
    ///
    /// Environment bindings accumulate rather than replace: the workspace map is
    /// layered on top of the global one, so a same-named binding takes the
    /// workspace value and everything else stays inherited.
    #[must_use]
    pub fn with_local(mut self, local: &LocalSettings) -> Self {
        if let Some(model) = local.default_model {
            self.default_model = model;
        }
        if let Some(enabled) = local.issue_enabled {
            self.issue_enabled = enabled;
        }
        if let Some(enabled) = local.memory_enabled {
            self.memory_enabled = enabled;
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

/// Per-workspace Agent, Issue, and Memory settings stored in
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
    pub issue_enabled: Option<bool>,
    pub memory_enabled: Option<bool>,
    /// Environment bindings this workspace adds to the global ones. An empty map
    /// means the workspace uses exactly what it inherits.
    pub env: EnvBindings,
}

impl LocalSettings {
    /// Replace the Agent, Issue, and Memory choices with `settings`, keeping this
    /// workspace's own environment bindings.
    ///
    /// The Config surface edits a merged [`Settings`] view, which carries the
    /// *inherited* environment; writing that view back verbatim would copy every
    /// global binding into the workspace file. Config saves therefore go through
    /// here so the workspace keeps owning only what it declared itself.
    #[must_use]
    pub fn with_config(mut self, settings: &Settings) -> Self {
        self.default_model = Some(settings.default_model);
        self.issue_enabled = Some(settings.issue_enabled);
        self.memory_enabled = Some(settings.memory_enabled);
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

#[cfg(test)]
mod tests;

//! Application settings.
//!
//! The global, per-user preferences persisted as `settings.json` in the data
//! directory, plus workspace settings persisted beside a project. Theme and
//! modal interaction stay global; Agent, Issue, and Memory values are copied to
//! a workspace when it is registered and may then be changed independently.
//!
//! Enum-valued settings degrade an unrecognised stored token to a sensible
//! default rather than failing the whole file, so a value written by a newer
//! usagi — or a hand-edited typo — never blocks loading. [`Theme`] does this with
//! `#[serde(other)]` on [`Theme::System`] (unknown → follow the OS).

use serde::{Deserialize, Serialize};

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

/// The cloud model provider used when a new Agent pane has no explicit profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefaultModel {
    /// Anthropic Claude, launched through the `claude` profile.
    Claude,
    /// `OpenAI`, launched through the Codex `codex` profile.
    #[default]
    #[serde(rename = "openai", other)]
    OpenAi,
}

impl DefaultModel {
    /// Stable daemon profile ID selected by this model provider.
    #[must_use]
    pub const fn profile_id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::OpenAi => "codex",
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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: Theme::default(),
            modal_selection_mode: ModalSelectionMode::default(),
            default_model: DefaultModel::default(),
            issue_enabled: true,
            memory_enabled: true,
        }
    }
}

impl Settings {
    /// Apply workspace-owned Agent, Issue, and Memory values over this global
    /// baseline. Theme and modal interaction always remain global.
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
        self
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
}

impl From<&Settings> for LocalSettings {
    fn from(settings: &Settings) -> Self {
        Self {
            default_model: Some(settings.default_model),
            issue_enabled: Some(settings.issue_enabled),
            memory_enabled: Some(settings.memory_enabled),
        }
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
        _ => None,
    })
}

#[cfg(test)]
mod tests;

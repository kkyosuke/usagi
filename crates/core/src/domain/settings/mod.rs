//! Application settings.
//!
//! The global, per-user preferences persisted as `settings.json` in the data
//! directory, plus optional per-workspace overrides persisted beside a project.
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
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The UI color theme.
    pub theme: Theme,
    /// The command-selection interaction used by Overview and Closeup modals.
    pub modal_selection_mode: ModalSelectionMode,
    /// The provider used for Agent panes when no profile is selected explicitly.
    pub default_model: DefaultModel,
}

impl Settings {
    /// Apply the fields explicitly set by `local` over this global baseline.
    #[must_use]
    pub fn with_local(mut self, local: &LocalSettings) -> Self {
        if let Some(theme) = local.theme {
            self.theme = theme;
        }
        if let Some(mode) = local.modal_selection_mode {
            self.modal_selection_mode = mode;
        }
        if let Some(model) = local.default_model {
            self.default_model = model;
        }
        self
    }
}

/// Per-workspace overrides stored in `<workspace>/.usagi/settings.json` (or the
/// build-channel-specific `dev` directory).
///
/// An absent or unrecognised field defers to the global value. This keeps a
/// file written by a newer usagi safe to load without turning an unknown local
/// token into an unintended override.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalSettings {
    #[serde(deserialize_with = "deserialize_local_theme")]
    pub theme: Option<Theme>,
    #[serde(deserialize_with = "deserialize_local_modal_selection_mode")]
    pub modal_selection_mode: Option<ModalSelectionMode>,
    #[serde(deserialize_with = "deserialize_local_default_model")]
    pub default_model: Option<DefaultModel>,
}

impl From<&Settings> for LocalSettings {
    fn from(settings: &Settings) -> Self {
        Self {
            theme: Some(settings.theme),
            modal_selection_mode: Some(settings.modal_selection_mode),
            default_model: Some(settings.default_model),
        }
    }
}

fn deserialize_local_theme<'de, D>(deserializer: D) -> Result<Option<Theme>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let token = Option::<String>::deserialize(deserializer)?;
    Ok(match token.as_deref() {
        Some("light") => Some(Theme::Light),
        Some("dark") => Some(Theme::Dark),
        Some("system") => Some(Theme::System),
        _ => None,
    })
}

fn deserialize_local_modal_selection_mode<'de, D>(
    deserializer: D,
) -> Result<Option<ModalSelectionMode>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let token = Option::<String>::deserialize(deserializer)?;
    Ok(match token.as_deref() {
        Some("prompt") => Some(ModalSelectionMode::Prompt),
        Some("action") => Some(ModalSelectionMode::Action),
        _ => None,
    })
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

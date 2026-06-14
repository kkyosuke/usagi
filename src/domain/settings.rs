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
}

impl LocalSettings {
    /// Whether every field is unset, i.e. the project adds no local override.
    pub fn is_empty(&self) -> bool {
        self.agent_cli.is_none() && self.notifications_enabled.is_none()
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
        }
        .is_empty());
        assert!(!LocalSettings {
            agent_cli: None,
            notifications_enabled: Some(true),
        }
        .is_empty());
    }
}

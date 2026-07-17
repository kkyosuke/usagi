//! Workspace-owned runtime/model allowlists and executable lookup boundary.
//!
//! Both MCP schema publication and daemon launch admission use this module so
//! a snapshot can never become an authorization source.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::Path;

use serde::Deserialize;

const CONFIG_PATH: &str = ".usagi/config.toml";

/// PATH lookup boundary. Tests inject this port instead of depending on PATH.
pub trait ExecutableLocator {
    /// Whether `executable` can be run from the current PATH.
    fn is_available(&self, executable: &str) -> bool;
}

/// Production PATH lookup implementation.
pub struct PathExecutableLocator;

impl ExecutableLocator for PathExecutableLocator {
    #[coverage(off)] // Production PATH boundary; tests inject a fake locator.
    fn is_available(&self, executable: &str) -> bool {
        env::var_os("PATH")
            .is_some_and(|paths| env::split_paths(&paths).any(|dir| dir.join(executable).is_file()))
    }
}

#[derive(Debug, Default, Deserialize)]
struct WorkspaceConfig {
    #[serde(default)]
    agents: AgentsConfig,
}

#[derive(Debug, Default, Deserialize)]
struct AgentsConfig {
    #[serde(default)]
    claude: RuntimeConfig,
    #[serde(default)]
    codex: RuntimeConfig,
}

#[derive(Debug, Default, Deserialize)]
struct RuntimeConfig {
    #[serde(default)]
    models: Vec<String>,
}

/// Runtime/model configuration read from a workspace's `.usagi/config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkspaceAgentConfig {
    claude: Vec<String>,
    codex: Vec<String>,
}

impl WorkspaceAgentConfig {
    /// Builds an in-memory configuration for injected callers and tests.
    #[must_use]
    pub fn from_allowlists(claude: Vec<String>, codex: Vec<String>) -> Self {
        Self { claude, codex }
    }
    /// Read configuration. Missing or malformed input is an empty allowlist.
    #[must_use]
    pub fn read(workspace: &Path) -> Self {
        let Ok(text) = fs::read_to_string(workspace.join(CONFIG_PATH)) else {
            return Self::default();
        };
        let Ok(parsed) = toml::from_str::<WorkspaceConfig>(&text) else {
            return Self::default();
        };
        Self {
            claude: valid_models(parsed.agents.claude.models).unwrap_or_default(),
            codex: valid_models(parsed.agents.codex.models).unwrap_or_default(),
        }
    }

    /// Models allowed for this closed-vocabulary runtime.
    #[must_use]
    pub fn models(&self, runtime: &str) -> &[String] {
        match runtime {
            "claude" => &self.claude,
            "codex" => &self.codex,
            _ => &[],
        }
    }

    /// Whether the exact runtime/model pair is currently allowed.
    #[must_use]
    pub fn allows(&self, runtime: &str, model: &str) -> bool {
        self.models(runtime).iter().any(|allowed| allowed == model)
    }
}

fn valid_models(models: Vec<String>) -> Option<Vec<String>> {
    (!models.is_empty()
        && models
            .iter()
            .all(|model| !model.is_empty() && !model.chars().any(char::is_control))
        && models.iter().collect::<BTreeSet<_>>().len() == models.len())
    .then_some(models)
}

#[cfg(test)]
mod tests {
    use super::WorkspaceAgentConfig;
    use tempfile::tempdir;

    #[test]
    fn reader_admits_only_well_formed_runtime_specific_allowlists() {
        let workspace = tempdir().unwrap();
        std::fs::create_dir(workspace.path().join(".usagi")).unwrap();
        std::fs::write(
            workspace.path().join(".usagi/config.toml"),
            "[agents.claude]\nmodels = [\"sonnet\"]\n[agents.codex]\nmodels = [\"\", \"gpt\"]\n",
        )
        .unwrap();
        let config = WorkspaceAgentConfig::read(workspace.path());
        assert!(config.allows("claude", "sonnet"));
        assert!(!config.allows("claude", "opus"));
        assert!(config.models("codex").is_empty());
    }
}

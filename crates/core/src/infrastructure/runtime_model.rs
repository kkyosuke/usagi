//! Workspace-owned runtime/model allowlists and executable lookup boundary.
//!
//! Both MCP schema publication and daemon launch admission use this module so
//! a snapshot can never become an authorization source.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use serde::Deserialize;

use crate::domain::settings::{AvailableModels, DefaultModel};

const CONFIG_PATH: &str = ".usagi/config.toml";

/// One code-defined agent runtime exposed by daemon orchestration and MCP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupportedAgentRuntime {
    /// Stable daemon profile ID and MCP runtime token.
    pub id: &'static str,
    /// Executable whose PATH availability gates this runtime.
    pub executable: &'static str,
}

/// The agent runtime catalog shared by daemon registration and MCP dispatch.
///
/// [`DefaultModel::ALL`] is the closed-vocabulary `SSoT`; adding a provider there
/// automatically makes it a candidate on every catalog consumer.
#[must_use]
pub fn supported_agent_runtimes() -> impl ExactSizeIterator<Item = SupportedAgentRuntime> {
    DefaultModel::ALL
        .into_iter()
        .map(|model| SupportedAgentRuntime {
            id: model.profile_id(),
            executable: model.command(),
        })
}

/// PATH lookup boundary. Tests inject this port instead of depending on PATH.
pub trait ExecutableLocator: Send {
    /// Whether `executable` can be run from the current PATH.
    fn is_available(&self, executable: &str) -> bool;
}

/// Production PATH lookup implementation.
pub struct PathExecutableLocator;

impl ExecutableLocator for PathExecutableLocator {
    fn is_available(&self, executable: &str) -> bool {
        env::var_os("PATH").is_some_and(|paths| {
            env::split_paths(&paths).any(|dir| {
                let candidate = dir.join(executable);
                candidate.metadata().is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
            })
        })
    }
}

/// Captures the installed provider set without executing any provider CLI.
///
/// Callers retain this value for their process lifetime (or replace it only on
/// an explicit refresh), so every picker and validation surface observes one
/// stable snapshot.
#[must_use]
pub fn observe_available_models(locator: &dyn ExecutableLocator) -> AvailableModels {
    AvailableModels::new(
        DefaultModel::ALL
            .into_iter()
            .filter(|model| locator.is_available(model.command())),
    )
}

#[derive(Debug, Default, Deserialize)]
struct WorkspaceConfig {
    #[serde(default)]
    agents: BTreeMap<String, RuntimeConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct RuntimeConfig {
    #[serde(default)]
    models: Vec<String>,
}

/// Runtime/model configuration read from a workspace's `.usagi/config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkspaceAgentConfig {
    runtimes: BTreeMap<String, Vec<String>>,
}

impl WorkspaceAgentConfig {
    /// Builds an in-memory configuration for injected callers and tests.
    #[must_use]
    pub fn from_allowlists(claude: Vec<String>, codex: Vec<String>) -> Self {
        Self::from_runtime_allowlists([("claude", claude), ("codex", codex)])
    }

    /// Builds an in-memory configuration keyed by supported runtime ID.
    #[must_use]
    pub fn from_runtime_allowlists<'a>(
        allowlists: impl IntoIterator<Item = (&'a str, Vec<String>)>,
    ) -> Self {
        Self {
            runtimes: allowlists
                .into_iter()
                .filter(|(_, models)| !models.is_empty())
                .map(|(runtime, models)| (runtime.to_owned(), models))
                .collect(),
        }
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
        let runtimes = parsed
            .agents
            .into_iter()
            .filter(|(runtime, _)| supported_agent_runtimes().any(|entry| entry.id == runtime))
            .filter_map(|(runtime, config)| {
                valid_models(config.models).map(|models| (runtime, models))
            })
            .collect();
        Self { runtimes }
    }

    /// Models allowed for this closed-vocabulary runtime.
    #[must_use]
    pub fn models(&self, runtime: &str) -> &[String] {
        supported_agent_runtimes()
            .any(|entry| entry.id == runtime)
            .then(|| self.runtimes.get(runtime))
            .flatten()
            .map_or(&[], Vec::as_slice)
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
    use std::os::unix::fs::PermissionsExt as _;

    use crate::domain::settings::DefaultModel;

    use super::{
        ExecutableLocator, PathExecutableLocator, WorkspaceAgentConfig, observe_available_models,
        supported_agent_runtimes,
    };
    use tempfile::tempdir;

    #[test]
    fn reader_admits_only_well_formed_runtime_specific_allowlists() {
        let injected =
            WorkspaceAgentConfig::from_allowlists(vec!["opus".into()], vec!["gpt-5".into()]);
        assert!(injected.allows("claude", "opus"));
        assert!(injected.allows("codex", "gpt-5"));
        let injected_sakana = WorkspaceAgentConfig::from_runtime_allowlists([(
            "sakana-ai",
            vec!["fugu-model".into()],
        )]);
        assert!(injected_sakana.allows("sakana-ai", "fugu-model"));

        let workspace = tempdir().unwrap();
        std::fs::create_dir(workspace.path().join(".usagi")).unwrap();
        std::fs::write(
            workspace.path().join(".usagi/config.toml"),
            "[agents.claude]\nmodels = [\"sonnet\"]\n[agents.codex]\nmodels = [\"\", \"gpt\"]\n[agents.sakana-ai]\nmodels = [\"fugu-model\"]\n",
        )
        .unwrap();
        let config = WorkspaceAgentConfig::read(workspace.path());
        assert!(config.allows("claude", "sonnet"));
        assert!(!config.allows("claude", "opus"));
        assert!(config.models("codex").is_empty());
        assert!(config.allows("sakana-ai", "fugu-model"));

        assert!(
            WorkspaceAgentConfig::read(workspace.path().join("missing").as_path())
                .models("claude")
                .is_empty()
        );
        std::fs::write(workspace.path().join(".usagi/config.toml"), "not = [toml").unwrap();
        assert!(
            WorkspaceAgentConfig::read(workspace.path())
                .models("claude")
                .is_empty()
        );
        assert!(config.models("unknown").is_empty());
    }

    #[test]
    fn runtime_catalog_uses_profile_ids_and_executables_from_the_model_ssot() {
        let actual = supported_agent_runtimes()
            .map(|runtime| (runtime.id, runtime.executable))
            .collect::<Vec<_>>();
        assert_eq!(
            actual,
            vec![
                ("claude", "claude"),
                ("codex", "codex"),
                ("sakana-ai", "codex-fugu"),
            ]
        );
    }

    #[test]
    fn availability_snapshot_uses_the_locator_once_per_provider() {
        use std::sync::Mutex;

        struct RecordingLocator(Mutex<Vec<String>>);
        impl ExecutableLocator for RecordingLocator {
            fn is_available(&self, executable: &str) -> bool {
                self.0.lock().unwrap().push(executable.to_owned());
                executable == "codex"
            }
        }

        let locator = RecordingLocator(Mutex::new(Vec::new()));
        let available = observe_available_models(&locator);
        assert_eq!(
            available.iter().collect::<Vec<_>>(),
            vec![DefaultModel::OpenAi]
        );
        assert_eq!(
            *locator.0.lock().unwrap(),
            ["claude", "codex", "codex-fugu"]
        );
    }

    #[test]
    fn path_locator_finds_files_on_path_and_rejects_missing_names() {
        let _guard = crate::test_support::process_env_guard();
        let bin = tempdir().unwrap();
        std::fs::write(bin.path().join("usagi-test-runtime"), "").unwrap();
        std::fs::write(bin.path().join("not-executable"), "").unwrap();
        std::fs::set_permissions(
            bin.path().join("usagi-test-runtime"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let previous_path = std::env::var_os("PATH").expect("test process has PATH");
        unsafe {
            std::env::set_var("PATH", bin.path());
        }

        let locator = PathExecutableLocator;
        assert!(locator.is_available("usagi-test-runtime"));
        assert!(!locator.is_available("not-executable"));
        assert!(!locator.is_available("absent-runtime"));

        unsafe {
            std::env::set_var("PATH", previous_path);
        }
    }
}

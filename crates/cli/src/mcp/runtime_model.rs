//! Workspace-owned runtime/model allowlists used by the MCP dispatch surface.
//!
//! The snapshot is deliberately built once when an MCP server is created.  It
//! never asks a provider or an agent CLI for available models.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::Path;

use serde::Deserialize;
use serde_json::{Value, json};

const CONFIG_PATH: &str = ".usagi/config.toml";

/// PATH lookup boundary. Tests inject this port instead of depending on PATH.
pub trait ExecutableLocator {
    /// Whether `executable` can be run from the current PATH.
    fn is_available(&self, executable: &str) -> bool;
}

/// Production PATH lookup implementation.
pub struct PathExecutableLocator;

impl ExecutableLocator for PathExecutableLocator {
    #[coverage(off)] // Production PATH boundary; schema tests inject a fake locator.
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
    /// Read workspace configuration. A missing config is an empty allowlist;
    /// malformed configuration likewise publishes no runtime rather than
    /// guessing a safe selection.
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

    #[cfg(test)]
    pub(crate) fn with_models_for_test(claude: Vec<&str>, codex: Vec<&str>) -> Self {
        Self {
            claude: claude.into_iter().map(str::to_owned).collect(),
            codex: codex.into_iter().map(str::to_owned).collect(),
        }
    }

    fn models(&self, runtime: &str) -> &[String] {
        match runtime {
            "claude" => &self.claude,
            "codex" => &self.codex,
            _ => &[],
        }
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

/// Immutable runtime/model availability captured for one MCP server lifetime.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuntimeModelSnapshot {
    runtimes: Vec<RuntimeModels>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeModels {
    runtime: &'static str,
    models: Vec<String>,
}

impl RuntimeModelSnapshot {
    /// Capture config and executable availability once.
    #[must_use]
    pub fn capture(config: &WorkspaceAgentConfig, locator: &dyn ExecutableLocator) -> Self {
        let runtimes = ["claude", "codex"]
            .into_iter()
            .filter_map(|runtime| {
                let models = config.models(runtime);
                (locator.is_available(runtime) && !models.is_empty()).then(|| RuntimeModels {
                    runtime,
                    models: models.to_vec(),
                })
            })
            .collect();
        Self { runtimes }
    }

    /// Build the `agent` schema for `session_dispatch`.
    #[must_use]
    pub fn agent_schema(&self) -> Value {
        let mut branches = vec![json!({
            "type":"object", "properties":{"id":{"type":"string"}},
            "required":["id"], "additionalProperties":false
        })];
        branches.extend(self.runtimes.iter().map(|entry| {
            json!({
                "type":"object",
                "properties": {
                    "runtime":{"const":entry.runtime},
                    "model":{"type":"string", "enum":entry.models}
                },
                "required":["runtime", "model"], "additionalProperties":false
            })
        }));
        json!({"oneOf": branches})
    }

    /// Validate the new-agent selector carried by MCP arguments.
    ///
    /// # Errors
    ///
    /// Returns a safe validation message when the selector is incomplete,
    /// mixed, or absent from this snapshot.
    pub fn validate_agent(&self, agent: &Value) -> Result<(), String> {
        let object = agent
            .as_object()
            .ok_or_else(|| "agent must be an object".to_owned())?;
        let id = object.get("id");
        let runtime = object.get("runtime");
        let model = object.get("model");
        if id.is_some() {
            if runtime.is_some() || model.is_some() || object.len() != 1 {
                return Err("agent.id cannot be combined with runtime or model".into());
            }
            return id
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(|_| ())
                .ok_or_else(|| "agent.id must be a non-empty string".into());
        }
        let (Some(runtime), Some(model)) = (
            runtime.and_then(Value::as_str),
            model.and_then(Value::as_str),
        ) else {
            return Err("agent.runtime and agent.model must be supplied together".into());
        };
        if object.len() != 2 {
            return Err("agent must contain either id or runtime and model".into());
        }
        self.runtimes
            .iter()
            .find(|entry| entry.runtime == runtime)
            .filter(|entry| entry.models.iter().any(|allowed| allowed == model))
            .map(|_| ())
            .ok_or_else(|| "runtime/model is not allowed by this MCP server snapshot".into())
    }

    /// Validate and normalize deprecated `agent_cli` on legacy session tools.
    ///
    /// # Errors
    ///
    /// Returns a migration or allowlist validation message for invalid input.
    pub fn normalize_legacy_agent(&self, arguments: &mut Value) -> Result<(), String> {
        let object = arguments
            .as_object_mut()
            .ok_or_else(|| "arguments must be an object".to_owned())?;
        let alias = object.remove("agent_cli");
        if let Some(alias) = alias {
            if object.contains_key("runtime")
                || object
                    .get("agent")
                    .and_then(Value::as_object)
                    .is_some_and(|agent| agent.contains_key("id"))
            {
                return Err(
                    "agent_cli is deprecated and cannot be combined with runtime or agent.id"
                        .into(),
                );
            }
            let runtime = alias
                .as_str()
                .filter(|value| matches!(*value, "claude" | "codex"))
                .ok_or_else(|| "agent_cli must be claude or codex".to_owned())?;
            object.insert("runtime".into(), Value::String(runtime.into()));
        }
        if let Some(agent) = object.get("agent") {
            self.validate_agent(agent)?;
        }
        let runtime = object.get("runtime").and_then(Value::as_str);
        let model = object.get("model").and_then(Value::as_str);
        match (runtime, model) {
            (None, None) => Ok(()),
            (Some(runtime), Some(model))
                if self.runtimes.iter().any(|entry| {
                    entry.runtime == runtime && entry.models.iter().any(|allowed| allowed == model)
                }) =>
            {
                Ok(())
            }
            (Some(_), Some(_)) => {
                Err("runtime/model is not allowed by this MCP server snapshot".into())
            }
            _ => Err("runtime and model must be supplied together".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AgentsConfig, ExecutableLocator, RuntimeConfig, RuntimeModelSnapshot, WorkspaceAgentConfig,
        WorkspaceConfig,
    };
    use serde_json::json;
    use tempfile::tempdir;

    struct FakeLocator(&'static [&'static str]);
    impl ExecutableLocator for FakeLocator {
        fn is_available(&self, executable: &str) -> bool {
            self.0.contains(&executable)
        }
    }

    #[test]
    fn raw_workspace_config_defaults_to_no_runtime_allowlists() {
        assert!(WorkspaceConfig::default().agents.claude.models.is_empty());
        assert!(AgentsConfig::default().codex.models.is_empty());
        assert!(RuntimeConfig::default().models.is_empty());
    }

    #[test]
    fn snapshot_exposes_only_configured_available_runtimes() {
        let config = WorkspaceAgentConfig::with_models_for_test(vec!["sonnet"], vec!["gpt-5"]);
        for (available, expected) in [
            (&["claude"][..], vec!["claude"]),
            (&["codex"][..], vec!["codex"]),
            (&[][..], vec![]),
            (&["claude", "codex"][..], vec!["claude", "codex"]),
        ] {
            let schema =
                RuntimeModelSnapshot::capture(&config, &FakeLocator(available)).agent_schema();
            let actual: Vec<_> = schema["oneOf"]
                .as_array()
                .unwrap()
                .iter()
                .skip(1)
                .map(|branch| branch["properties"]["runtime"]["const"].as_str().unwrap())
                .collect();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn reader_accepts_only_well_formed_workspace_allowlists() {
        let workspace = tempdir().unwrap();
        std::fs::create_dir(workspace.path().join(".usagi")).unwrap();
        std::fs::write(
            workspace.path().join(".usagi/config.toml"),
            "[agents.claude]\nmodels = [\"sonnet\", \"opus\"]\n[agents.codex]\nmodels = [\"\", \"gpt-5\"]\n",
        )
        .unwrap();
        let snapshot = RuntimeModelSnapshot::capture(
            &WorkspaceAgentConfig::read(workspace.path()),
            &FakeLocator(&["claude", "codex"]),
        );
        let branches = snapshot.agent_schema()["oneOf"].as_array().unwrap().clone();
        assert_eq!(branches.len(), 2);
        assert_eq!(
            branches[1]["properties"]["model"]["enum"],
            json!(["sonnet", "opus"])
        );
        assert!(
            WorkspaceAgentConfig::read(workspace.path())
                .models("unknown")
                .is_empty()
        );

        std::fs::write(workspace.path().join(".usagi/config.toml"), "not = [valid").unwrap();
        assert_eq!(
            WorkspaceAgentConfig::read(workspace.path()),
            WorkspaceAgentConfig::default()
        );
    }

    #[test]
    fn snapshot_does_not_change_after_config_or_locator_changes() {
        let workspace = tempdir().unwrap();
        std::fs::create_dir(workspace.path().join(".usagi")).unwrap();
        std::fs::write(
            workspace.path().join(".usagi/config.toml"),
            "[agents.claude]\nmodels = [\"sonnet\"]\n",
        )
        .unwrap();
        let original_config = WorkspaceAgentConfig::read(workspace.path());
        let original = RuntimeModelSnapshot::capture(&original_config, &FakeLocator(&["claude"]));
        std::fs::write(
            workspace.path().join(".usagi/config.toml"),
            "[agents.codex]\nmodels = [\"gpt-5\"]\n",
        )
        .unwrap();
        let regenerated_config = WorkspaceAgentConfig::read(workspace.path());
        let regenerated =
            RuntimeModelSnapshot::capture(&regenerated_config, &FakeLocator(&["codex"]));
        assert_eq!(
            original.agent_schema()["oneOf"].as_array().unwrap().len(),
            2
        );
        assert_eq!(
            regenerated.agent_schema()["oneOf"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            original.agent_schema()["oneOf"][1]["properties"]["runtime"]["const"],
            "claude"
        );
        assert_eq!(
            regenerated.agent_schema()["oneOf"][1]["properties"]["runtime"]["const"],
            "codex"
        );
    }

    #[test]
    fn parser_rejects_invalid_and_mixed_agent_selectors() {
        let snapshot = RuntimeModelSnapshot::capture(
            &WorkspaceAgentConfig::with_models_for_test(vec!["sonnet"], vec![]),
            &FakeLocator(&["claude"]),
        );
        assert!(
            snapshot
                .validate_agent(&json!({"runtime":"claude","model":"sonnet"}))
                .is_ok()
        );
        assert!(snapshot.validate_agent(&json!("not-an-object")).is_err());
        for agent in [
            json!({"runtime":"claude"}),
            json!({"runtime":"claude","model":"opus"}),
            json!({"id":"a","runtime":"claude","model":"sonnet"}),
            json!({"id":""}),
            json!({"runtime":"claude","model":"sonnet","extra":true}),
        ] {
            assert!(snapshot.validate_agent(&agent).is_err());
        }
    }

    #[test]
    fn legacy_alias_normalizes_and_rejects_migration_mixes() {
        let snapshot = RuntimeModelSnapshot::capture(
            &WorkspaceAgentConfig::with_models_for_test(vec!["sonnet"], vec![]),
            &FakeLocator(&["claude"]),
        );
        let mut accepted = json!({"agent_cli":"claude", "model":"sonnet"});
        snapshot.normalize_legacy_agent(&mut accepted).unwrap();
        assert_eq!(accepted["runtime"], "claude");
        let mut existing = json!({"agent":{"id":"existing"}});
        assert!(snapshot.normalize_legacy_agent(&mut existing).is_ok());
        let mut non_object = json!("not-an-object");
        assert!(snapshot.normalize_legacy_agent(&mut non_object).is_err());
        let mut incomplete_agent = json!({"agent":{"runtime":"claude"}});
        assert!(
            snapshot
                .normalize_legacy_agent(&mut incomplete_agent)
                .is_err()
        );
        for mut value in [
            json!({"agent_cli":"claude","runtime":"claude","model":"sonnet"}),
            json!({"agent_cli":"claude","agent":{"id":"a"}}),
            json!({"runtime":"claude","model":"opus"}),
            json!({"runtime":"claude"}),
            json!({"agent_cli":"unknown","model":"sonnet"}),
        ] {
            assert!(snapshot.normalize_legacy_agent(&mut value).is_err());
        }
    }
}

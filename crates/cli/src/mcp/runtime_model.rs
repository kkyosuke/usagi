//! Workspace-owned runtime/model allowlists used by the MCP dispatch surface.
//!
//! The snapshot is deliberately built once when an MCP server is created.  It
//! never asks a provider or an agent CLI for available models.

use serde_json::{Value, json};
pub use usagi_core::infrastructure::runtime_model::{
    ExecutableLocator, PathExecutableLocator, WorkspaceAgentConfig,
};

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
    use super::{ExecutableLocator, RuntimeModelSnapshot, WorkspaceAgentConfig};
    use serde_json::json;
    use tempfile::tempdir;

    struct FakeLocator(&'static [&'static str]);
    impl ExecutableLocator for FakeLocator {
        fn is_available(&self, executable: &str) -> bool {
            self.0.contains(&executable)
        }
    }

    #[test]
    fn default_workspace_config_has_no_runtime_allowlists() {
        assert_eq!(
            WorkspaceAgentConfig::default(),
            WorkspaceAgentConfig::from_allowlists(vec![], vec![])
        );
    }

    #[test]
    fn snapshot_exposes_only_configured_available_runtimes() {
        let config =
            WorkspaceAgentConfig::from_allowlists(vec!["sonnet".into()], vec!["gpt-5".into()]);
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
            &WorkspaceAgentConfig::from_allowlists(vec!["sonnet".into()], vec![]),
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
            &WorkspaceAgentConfig::from_allowlists(vec!["sonnet".into()], vec![]),
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

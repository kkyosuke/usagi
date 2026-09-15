//! Workspace-owned runtime/model allowlists, session setup, and executable lookup.
//!
//! Both MCP schema publication and daemon launch admission use this module so
//! a snapshot can never become an authorization source.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use anyhow::Context as _;
use serde::Deserialize;
use toml_edit::{Array, DocumentMut, Item, Table, value};

use crate::domain::settings::{AvailableModels, DefaultModel};
use crate::infrastructure::persistence::{json_file, store_lock::StoreLock};

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

/// The credential names bound in the user's global settings.
///
/// Only the *names* are carried. Whether a provider can be offered is decided
/// by whether its key is configured at all, never by its value, so no secret is
/// read — and an `op://` reference stays unresolved until a launch needs it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BoundCredentials(BTreeSet<String>);

impl BoundCredentials {
    /// Collect the bound names. Values are deliberately not accepted.
    #[must_use]
    pub fn new<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self(names.into_iter().map(Into::into).collect())
    }

    /// The names bound in the user's global settings.
    ///
    /// Global scope is the whole story for a provider credential: every name a
    /// provider injects is reserved from workspace bindings, so a checked-in
    /// `.usagi/settings.json` can neither supply one nor take one away.
    #[must_use]
    pub fn from_settings(settings: &crate::domain::settings::Settings) -> Self {
        Self(
            settings
                .env_bindings()
                .map(|(name, _)| name.to_owned())
                .collect(),
        )
    }

    /// Whether `name` is bound.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.0.contains(name)
    }
}

/// Captures the selectable provider set without executing any provider CLI.
///
/// A provider is selectable when its CLI is installed **and** every credential
/// it declares through [`DefaultModel::credential_binding`] is configured. The
/// second half is not a refinement: `sakana-ai` is Fugu served through the same
/// `claude` executable, so a PATH lookup alone reports it installed on every
/// machine that has Claude Code, and every picker would offer a provider the
/// daemon refuses to launch because no `SAKANA_API_KEY` exists. Availability is
/// per provider, so it is decided by what that provider needs.
///
/// Callers retain this value for their process lifetime (or replace it only on
/// an explicit refresh), so every picker and validation surface observes one
/// stable snapshot.
#[must_use]
pub fn observe_available_models(
    locator: &dyn ExecutableLocator,
    credentials: &BoundCredentials,
) -> AvailableModels {
    AvailableModels::new(DefaultModel::ALL.into_iter().filter(|model| {
        locator.is_available(model.command())
            && model
                .credential_binding()
                .is_none_or(|(source, _)| credentials.contains(source))
    }))
}

#[derive(Debug, Default, Deserialize)]
struct WorkspaceConfig {
    #[serde(default)]
    agents: BTreeMap<String, RuntimeConfig>,
    #[serde(default)]
    session: SessionConfig,
}

#[derive(Debug, Default, Deserialize)]
struct RuntimeConfig {
    #[serde(default)]
    models: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct SessionConfig {
    #[serde(default)]
    setup_commands: Vec<String>,
}

/// Agent configuration read from `.usagi/config.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceAgentConfig {
    runtimes: BTreeMap<String, Vec<String>>,
}

impl Default for WorkspaceAgentConfig {
    fn default() -> Self {
        // A freshly initialized workspace must be able to delegate without an
        // undocumented config edit. `default` is the provider-owned selector:
        // the CLI, rather than usagi, chooses the concrete Claude model.
        Self::from_runtime_allowlists([("claude", vec!["default".to_owned()])])
    }
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
    /// Read configuration. A missing file uses the product default; malformed
    /// or explicitly empty configuration remains fail-closed.
    #[must_use]
    pub fn read(workspace: &Path) -> Self {
        let text = match fs::read_to_string(workspace.join(CONFIG_PATH)) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(_) => return Self::empty(),
        };
        let Ok(parsed) = toml::from_str::<WorkspaceConfig>(&text) else {
            return Self::empty();
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

    /// Builds an explicit deny-all policy.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            runtimes: BTreeMap::new(),
        }
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

/// Session configuration read from `.usagi/config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceSessionConfig {
    setup_commands: Vec<String>,
}

impl WorkspaceSessionConfig {
    /// Reads session configuration, defaulting to no setup commands when the
    /// file is missing, unreadable, or malformed.
    #[must_use]
    pub fn read(workspace: &Path) -> Self {
        Self::load(workspace).unwrap_or_default()
    }

    /// Reads session configuration while preserving malformed or unreadable
    /// files as errors for interactive editors.
    ///
    /// # Errors
    ///
    /// Returns an error when an existing config cannot be read or parsed.
    pub fn load(workspace: &Path) -> anyhow::Result<Self> {
        let path = workspace.join(CONFIG_PATH);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(anyhow::Error::new(error))
                    .context(format!("failed to read {}", path.display()));
            }
        };
        let parsed = toml::from_str::<WorkspaceConfig>(&text)
            .context(format!("failed to parse {}", path.display()))?;
        Ok(Self {
            setup_commands: normalize_setup_commands(parsed.session.setup_commands),
        })
    }

    /// Atomically replaces only `[session].setup_commands`, preserving all
    /// other TOML values, ordering, and comments in the workspace config.
    ///
    /// # Errors
    ///
    /// Returns an error when the config is unreadable or malformed, when
    /// `[session]` is not a table, or when the locked atomic write fails.
    pub fn save_setup_commands(workspace: &Path, setup_commands: &[String]) -> anyhow::Result<()> {
        let path = workspace.join(CONFIG_PATH);
        let _lock = StoreLock::acquire(&workspace.join(".usagi"))?;
        let mut document = match fs::read_to_string(&path) {
            Ok(text) => text
                .parse::<DocumentMut>()
                .context(format!("failed to parse {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => DocumentMut::new(),
            Err(error) => {
                return Err(anyhow::Error::new(error))
                    .context(format!("failed to read {}", path.display()));
            }
        };
        if !document.contains_key("session") {
            document["session"] = Item::Table(Table::new());
        }
        let session = document["session"]
            .as_table_mut()
            .context("workspace config [session] must be a table")?;
        let mut commands = Array::new();
        for command in normalize_setup_commands(setup_commands.iter().cloned()) {
            commands.push(command);
        }
        session["setup_commands"] = value(commands);
        json_file::write_text_atomic(&path, &document.to_string())
    }

    /// Shell command lines run in order after a managed session worktree is built.
    #[must_use]
    pub fn setup_commands(&self) -> &[String] {
        &self.setup_commands
    }
}

fn normalize_setup_commands(commands: impl IntoIterator<Item = String>) -> Vec<String> {
    commands
        .into_iter()
        .filter(|command| !command.trim().is_empty())
        .collect()
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
        BoundCredentials, ExecutableLocator, PathExecutableLocator, WorkspaceAgentConfig,
        WorkspaceSessionConfig, observe_available_models, supported_agent_runtimes,
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
            "[session]\nsetup_commands = [\"first\", \"  \", \"second\"]\n[agents.claude]\nmodels = [\"sonnet\"]\n[agents.codex]\nmodels = [\"\", \"gpt\"]\n[agents.sakana-ai]\nmodels = [\"fugu-model\"]\n",
        )
        .unwrap();
        let config = WorkspaceAgentConfig::read(workspace.path());
        assert!(config.allows("claude", "sonnet"));
        assert!(!config.allows("claude", "opus"));
        assert!(config.models("codex").is_empty());
        assert!(config.allows("sakana-ai", "fugu-model"));
        assert_eq!(
            WorkspaceSessionConfig::read(workspace.path()).setup_commands(),
            ["first", "second"]
        );

        assert!(
            WorkspaceAgentConfig::read(workspace.path().join("missing").as_path())
                .allows("claude", "default")
        );
        assert!(
            WorkspaceSessionConfig::read(workspace.path().join("missing").as_path())
                .setup_commands()
                .is_empty()
        );
        std::fs::write(workspace.path().join(".usagi/config.toml"), "not = [toml").unwrap();
        assert!(
            WorkspaceAgentConfig::read(workspace.path())
                .models("claude")
                .is_empty()
        );
        assert!(
            WorkspaceSessionConfig::read(workspace.path())
                .setup_commands()
                .is_empty()
        );
        std::fs::remove_file(workspace.path().join(".usagi/config.toml")).unwrap();
        std::fs::create_dir(workspace.path().join(".usagi/config.toml")).unwrap();
        assert!(
            WorkspaceAgentConfig::read(workspace.path())
                .models("claude")
                .is_empty()
        );
        assert!(WorkspaceSessionConfig::load(workspace.path()).is_err());
        assert!(
            WorkspaceSessionConfig::save_setup_commands(
                workspace.path(),
                &["cargo test".to_owned()]
            )
            .is_err()
        );
        assert!(config.models("unknown").is_empty());
    }

    #[test]
    fn session_setup_writer_preserves_other_toml_and_round_trips_atomically() {
        let workspace = tempdir().unwrap();
        assert!(
            WorkspaceSessionConfig::load(workspace.path())
                .unwrap()
                .setup_commands()
                .is_empty()
        );
        WorkspaceSessionConfig::save_setup_commands(
            workspace.path(),
            &["npm install".to_owned(), "  ".to_owned()],
        )
        .unwrap();
        assert_eq!(
            WorkspaceSessionConfig::load(workspace.path())
                .unwrap()
                .setup_commands(),
            ["npm install"]
        );

        let path = workspace.path().join(".usagi/config.toml");
        std::fs::write(
            &path,
            "# keep this comment\n[agents.claude]\nmodels = [\"sonnet\"]\n\n[session]\n# setup note\nsetup_commands = [\"old\"]\n",
        )
        .unwrap();
        WorkspaceSessionConfig::save_setup_commands(
            workspace.path(),
            &["cargo fetch".to_owned(), "cargo test".to_owned()],
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep this comment"));
        assert!(text.contains("# setup note"));
        assert!(text.contains("models = [\"sonnet\"]"));
        assert_eq!(
            WorkspaceSessionConfig::load(workspace.path())
                .unwrap()
                .setup_commands(),
            ["cargo fetch", "cargo test"]
        );
        assert!(WorkspaceAgentConfig::read(workspace.path()).allows("claude", "sonnet"));
    }

    #[test]
    fn session_setup_writer_rejects_malformed_or_non_table_session_without_overwriting() {
        let workspace = tempdir().unwrap();
        let dir = workspace.path().join(".usagi");
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("config.toml");

        for source in ["not = [toml", "session = \"invalid\"\n"] {
            std::fs::write(&path, source).unwrap();
            assert!(WorkspaceSessionConfig::load(workspace.path()).is_err());
            assert!(
                WorkspaceSessionConfig::save_setup_commands(
                    workspace.path(),
                    &["cargo test".to_owned()]
                )
                .is_err()
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        }
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
                ("sakana-ai", "claude"),
                ("agy", "agy"),
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
        let available = observe_available_models(&locator, &BoundCredentials::default());
        assert_eq!(
            available.iter().collect::<Vec<_>>(),
            vec![DefaultModel::OpenAi]
        );
        // `sakana-ai` is Fugu served through the same Claude CLI, so the
        // snapshot asks about `claude` once per provider rather than once per
        // distinct executable — availability is per provider.
        assert_eq!(
            *locator.0.lock().unwrap(),
            ["claude", "codex", "claude", "agy"]
        );
    }

    #[test]
    fn a_provider_that_declares_a_credential_is_offered_only_once_it_is_configured() {
        struct InstalledLocator;
        impl ExecutableLocator for InstalledLocator {
            fn is_available(&self, _executable: &str) -> bool {
                true
            }
        }

        // Claude Code is installed, so the shared `claude` executable says
        // nothing about whether Fugu is set up.
        assert_eq!(
            observe_available_models(&InstalledLocator, &BoundCredentials::default())
                .iter()
                .collect::<Vec<_>>(),
            vec![
                DefaultModel::Claude,
                DefaultModel::OpenAi,
                DefaultModel::Agy
            ]
        );
        let unrelated = BoundCredentials::new(["GH_TOKEN"]);
        assert!(!unrelated.contains("SAKANA_API_KEY"));
        assert_eq!(
            observe_available_models(&InstalledLocator, &unrelated)
                .iter()
                .collect::<Vec<_>>(),
            vec![
                DefaultModel::Claude,
                DefaultModel::OpenAi,
                DefaultModel::Agy
            ]
        );
        let configured = BoundCredentials::new(["SAKANA_API_KEY".to_owned()]);
        assert_eq!(
            observe_available_models(&InstalledLocator, &configured)
                .iter()
                .collect::<Vec<_>>(),
            DefaultModel::ALL
        );
    }

    #[test]
    fn bound_credentials_read_the_usable_global_bindings_only() {
        let mut settings = crate::domain::settings::Settings::default();
        assert!(!BoundCredentials::from_settings(&settings).contains("SAKANA_API_KEY"));
        settings.env = crate::domain::settings::EnvBindings::from([
            (
                " SAKANA_API_KEY ".to_owned(),
                " op://Private/Sakana/key ".to_owned(),
            ),
            ("BLANK".to_owned(), "   ".to_owned()),
        ]);
        let credentials = BoundCredentials::from_settings(&settings);
        // A name is enough: the reference is resolved by the launch, not by the
        // picker, so no secret is read to decide what to offer.
        assert!(credentials.contains("SAKANA_API_KEY"));
        assert!(!credentials.contains("BLANK"));
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

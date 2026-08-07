//! The configured environment the daemon injects into its PTY children.
//!
//! usagi stores environment bindings in two settings files — the per-user
//! `settings.json` under the data directory and each workspace's
//! `<workspace>/.usagi/settings.json` — and merges them with the workspace on
//! top ([`Settings::with_local`]). The daemon owns every PTY spawn, so it reads
//! that configuration itself at launch time: no environment value, and no
//! secret, is ever a client request field or an IPC payload
//! ([4. IPC](../../document/04-ipc.md)).
//!
//! A literal binding is injected as-is; a `op://…` binding is read through the
//! 1Password CLI. Resolved values are cached per workspace and reused while the
//! configuration is unchanged, so opening several panes runs `op read` once
//! rather than once per pane. Editing the configuration changes the cache key,
//! so the next launch resolves again — and a pane already running keeps the
//! environment it started with.
//!
//! A binding that cannot be resolved is dropped and logged: a locked vault
//! leaves one variable unset instead of making a pane impossible to open.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use usagi_core::domain::agent::EnvironmentVariableName;
use usagi_core::domain::settings::{EnvBindings, EnvLimitError, Settings, validate_env_limits};
use usagi_core::infrastructure::env_resolver::{
    OpCli, resolve_parallel_with_service_account_token,
};
use usagi_core::infrastructure::error_log::ErrorLog;
use usagi_core::infrastructure::store::settings::WorkspaceSettingsStore;
use usagi_core::infrastructure::store::workspace::Storage;
use usagi_core::usecase::env::SecretResolver;

const OP_SERVICE_ACCOUNT_TOKEN: &str = "OP_SERVICE_ACCOUNT_TOKEN";

#[derive(Clone, PartialEq, Eq)]
struct ConfiguredEnvironment {
    bindings: EnvBindings,
    service_account_token: Option<String>,
}

/// Resolved environment values, keyed by the complete configuration that
/// produced them, including the credential used only by `op read`.
type CachedEnvironment = (ConfiguredEnvironment, BTreeMap<String, String>);

/// Admission failures raised before any configured secret is resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserEnvironmentError {
    Limits(EnvLimitError),
    ReservedLauncherVariable,
}

impl From<EnvLimitError> for UserEnvironmentError {
    fn from(error: EnvLimitError) -> Self {
        Self::Limits(error)
    }
}

const CLAUDE_LAUNCHER_CONTROL_VARIABLES: [&str; 4] = [
    "PATH",
    "TMPDIR",
    "HOME",
    usagi_core::usecase::claude_sandbox::PASSTHROUGH_ENVIRONMENT_VARIABLE,
];

/// Resolved environment values for the configured bindings of one workspace.
pub struct UserEnvironment<R = OpCli> {
    global: Storage,
    resolver: R,
    /// Per workspace root: the bindings that produced the cached values, and the
    /// values themselves. A configuration change invalidates the entry because
    /// the stored bindings no longer match what the settings files hold.
    cache: Mutex<BTreeMap<PathBuf, CachedEnvironment>>,
}

impl<R: SecretResolver + Sync> UserEnvironment<R> {
    /// Read settings from `data_dir` and resolve secrets through `resolver`.
    pub fn new(data_dir: PathBuf, resolver: R) -> Self {
        Self {
            global: Storage::new(data_dir),
            resolver,
            cache: Mutex::new(BTreeMap::new()),
        }
    }

    /// The effective bindings for `workspace_root`: the global ones with the
    /// workspace's own layered on top. An unreadable settings file is logged and
    /// treated as "nothing configured", so a damaged file never blocks a launch.
    fn configured(
        &self,
        workspace_root: &Path,
    ) -> Result<ConfiguredEnvironment, UserEnvironmentError> {
        let global = match self.global.load_settings() {
            Ok(settings) => settings,
            Err(error) => {
                if let Some(limit) = error.downcast_ref::<EnvLimitError>() {
                    return Err((*limit).into());
                }
                ErrorLog::record(&format!("could not read global settings for env: {error}"));
                Settings::default()
            }
        };
        let local = match WorkspaceSettingsStore::new(workspace_root).load() {
            Ok(settings) => settings,
            Err(error) => {
                if let Some(limit) = error.downcast_ref::<EnvLimitError>() {
                    return Err((*limit).into());
                }
                ErrorLog::record(&format!(
                    "could not read workspace settings for env: {error}"
                ));
                usagi_core::domain::settings::LocalSettings::default()
            }
        };
        if local
            .env
            .keys()
            .any(|name| CLAUDE_LAUNCHER_CONTROL_VARIABLES.contains(&name.as_str()))
        {
            return Err(UserEnvironmentError::ReservedLauncherVariable);
        }
        let mut bindings = global.with_local(&local).env;
        validate_env_limits(&bindings)?;
        let service_account_token = bindings.remove(OP_SERVICE_ACCOUNT_TOKEN);
        Ok(ConfiguredEnvironment {
            bindings,
            service_account_token,
        })
    }

    /// The environment values to inject for a launch in `workspace_root`.
    pub fn resolved(
        &self,
        workspace_root: &Path,
    ) -> Result<BTreeMap<String, String>, UserEnvironmentError> {
        let configured = self.configured(workspace_root)?;
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((cached_configuration, values)) = cache.get(workspace_root)
            && *cached_configuration == configured
        {
            return Ok(values.clone());
        }
        let resolved = resolve_parallel_with_service_account_token(
            &configured.bindings,
            &self.resolver,
            configured.service_account_token.as_deref(),
        )
        .expect("removing one binding preserves the validated env limits");
        for failure in &resolved.failures {
            ErrorLog::record(&format!(
                "could not resolve environment variable {} from {}: {}",
                failure.name, failure.reference, failure.error
            ));
        }
        cache.insert(
            workspace_root.to_path_buf(),
            (configured, resolved.values.clone()),
        );
        Ok(resolved.values)
    }
}

/// The typed names of `values`, for a launch's environment allowlist.
///
/// Names come from [`valid_bindings`](usagi_core::domain::settings::valid_bindings),
/// which enforces the same rule [`EnvironmentVariableName`] does, so nothing is
/// dropped here in practice; an unexpected name is skipped rather than panicking
/// a launch.
pub fn allowlist(values: &BTreeMap<String, String>) -> BTreeSet<EnvironmentVariableName> {
    values
        .keys()
        .filter_map(|name| EnvironmentVariableName::new(name.clone()).ok())
        .collect()
}

/// The typed bindings of `values`, for an adapter's spawn provision.
pub fn typed(values: &BTreeMap<String, String>) -> Vec<(EnvironmentVariableName, String)> {
    values
        .iter()
        .filter_map(|(name, value)| {
            EnvironmentVariableName::new(name.clone())
                .ok()
                .map(|name| (name, value.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{UserEnvironment, UserEnvironmentError, allowlist, typed};
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::Mutex;
    use usagi_core::domain::settings::{EnvBindings, LocalSettings, Settings};
    use usagi_core::infrastructure::store::settings::WorkspaceSettingsStore;
    use usagi_core::infrastructure::store::workspace::Storage;
    use usagi_core::usecase::env::SecretResolver;

    struct CountingResolver {
        reads: Mutex<Vec<String>>,
        service_account_tokens: Mutex<Vec<Option<String>>>,
    }

    impl CountingResolver {
        fn new() -> Self {
            Self {
                reads: Mutex::new(Vec::new()),
                service_account_tokens: Mutex::new(Vec::new()),
            }
        }
        fn reads(&self) -> Vec<String> {
            self.reads.lock().unwrap().clone()
        }
        fn service_account_tokens(&self) -> Vec<Option<String>> {
            self.service_account_tokens.lock().unwrap().clone()
        }
    }

    impl SecretResolver for CountingResolver {
        fn read(&self, reference: &str) -> Result<String, String> {
            self.read_with_service_account_token(reference, None)
        }

        fn read_with_service_account_token(
            &self,
            reference: &str,
            service_account_token: Option<&str>,
        ) -> Result<String, String> {
            self.reads.lock().unwrap().push(reference.to_owned());
            self.service_account_tokens
                .lock()
                .unwrap()
                .push(service_account_token.map(str::to_owned));
            if reference.contains("Locked") {
                Err("op is locked".to_owned())
            } else {
                Ok(format!("secret:{reference}"))
            }
        }
    }

    fn bindings(pairs: &[(&str, &str)]) -> EnvBindings {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn write_global(data_dir: &Path, env: EnvBindings) {
        Storage::new(data_dir.to_path_buf())
            .save_settings(&Settings {
                env,
                ..Settings::default()
            })
            .unwrap();
    }

    fn write_workspace(workspace: &Path, env: EnvBindings) {
        let store = WorkspaceSettingsStore::new(workspace);
        store
            .save(&LocalSettings {
                env,
                ..LocalSettings::default()
            })
            .unwrap();
    }

    #[test]
    fn merges_both_scopes_resolves_secrets_and_reuses_the_resolution() {
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        write_global(
            data.path(),
            bindings(&[
                ("GH_TOKEN", "op://Private/GitHub/token"),
                ("RUST_LOG", "info"),
            ]),
        );
        write_workspace(
            workspace.path(),
            bindings(&[("RUST_LOG", "debug"), ("PROJECT", "usagi")]),
        );
        let environment = UserEnvironment::new(data.path().to_path_buf(), CountingResolver::new());

        let first = environment.resolved(workspace.path()).unwrap();
        assert_eq!(
            first,
            BTreeMap::from([
                (
                    "GH_TOKEN".to_owned(),
                    "secret:op://Private/GitHub/token".to_owned()
                ),
                ("PROJECT".to_owned(), "usagi".to_owned()),
                // The workspace value wins over the global one.
                ("RUST_LOG".to_owned(), "debug".to_owned()),
            ])
        );

        // A second launch with unchanged configuration reads no secret again.
        assert_eq!(environment.resolved(workspace.path()).unwrap(), first);
        assert_eq!(
            environment.resolver.reads(),
            ["op://Private/GitHub/token"],
            "the cached resolution is reused"
        );

        // Editing the configuration invalidates the cache.
        write_workspace(workspace.path(), bindings(&[("RUST_LOG", "trace")]));
        assert_eq!(
            environment
                .resolved(workspace.path())
                .unwrap()
                .get("RUST_LOG"),
            Some(&"trace".to_owned())
        );
        assert_eq!(environment.resolver.reads().len(), 2);
    }

    #[test]
    fn service_account_token_authenticates_op_only_and_workspace_overrides_global() {
        let direct = CountingResolver::new();
        assert_eq!(direct.read("literal"), Ok("secret:literal".to_owned()));
        assert_eq!(direct.service_account_tokens(), [None]);

        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        write_global(
            data.path(),
            bindings(&[
                ("GH_TOKEN", "op://Private/GitHub/token"),
                ("OP_SERVICE_ACCOUNT_TOKEN", "global-token"),
            ]),
        );
        let environment = UserEnvironment::new(data.path().to_path_buf(), CountingResolver::new());

        let values = environment.resolved(workspace.path()).unwrap();
        assert_eq!(values["GH_TOKEN"], "secret:op://Private/GitHub/token");
        assert!(!values.contains_key("OP_SERVICE_ACCOUNT_TOKEN"));
        assert_eq!(
            environment.resolver.service_account_tokens(),
            [Some("global-token".to_owned())]
        );

        write_workspace(
            workspace.path(),
            bindings(&[("OP_SERVICE_ACCOUNT_TOKEN", "workspace-token")]),
        );
        environment.resolved(workspace.path()).unwrap();
        assert_eq!(
            environment.resolver.service_account_tokens(),
            [
                Some("global-token".to_owned()),
                Some("workspace-token".to_owned()),
            ]
        );

        write_workspace(
            workspace.path(),
            bindings(&[("OP_SERVICE_ACCOUNT_TOKEN", "rotated-token")]),
        );
        environment.resolved(workspace.path()).unwrap();
        assert_eq!(
            environment.resolver.service_account_tokens(),
            [
                Some("global-token".to_owned()),
                Some("workspace-token".to_owned()),
                Some("rotated-token".to_owned()),
            ],
            "changing only the credential must invalidate the resolution cache"
        );
    }

    #[test]
    fn caches_each_workspace_separately() {
        let data = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        write_global(data.path(), bindings(&[("SHARED", "yes")]));
        write_workspace(first.path(), bindings(&[("WHICH", "first")]));
        write_workspace(second.path(), bindings(&[("WHICH", "second")]));
        let environment = UserEnvironment::new(data.path().to_path_buf(), CountingResolver::new());

        assert_eq!(
            environment.resolved(first.path()).unwrap(),
            BTreeMap::from([
                ("SHARED".to_owned(), "yes".to_owned()),
                ("WHICH".to_owned(), "first".to_owned()),
            ])
        );
        assert_eq!(
            environment.resolved(second.path()).unwrap(),
            BTreeMap::from([
                ("SHARED".to_owned(), "yes".to_owned()),
                ("WHICH".to_owned(), "second".to_owned()),
            ])
        );
    }

    #[test]
    fn an_unresolvable_binding_is_dropped_and_the_rest_is_injected() {
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        write_global(
            data.path(),
            bindings(&[("LOCKED", "op://Private/Locked/token"), ("PLAIN", "value")]),
        );
        let environment = UserEnvironment::new(data.path().to_path_buf(), CountingResolver::new());

        assert_eq!(
            environment.resolved(workspace.path()).unwrap(),
            BTreeMap::from([("PLAIN".to_owned(), "value".to_owned())])
        );
    }

    #[test]
    fn unreadable_settings_leave_the_environment_empty() {
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(data.path().join("settings.json"), "{ broken").unwrap();
        let local = WorkspaceSettingsStore::new(workspace.path());
        std::fs::create_dir_all(local.path().parent().unwrap()).unwrap();
        std::fs::write(local.path(), "{ broken").unwrap();
        let environment = UserEnvironment::new(data.path().to_path_buf(), CountingResolver::new());

        assert!(environment.resolved(workspace.path()).unwrap().is_empty());
    }

    #[test]
    fn merged_limit_is_refused_at_admission_with_zero_secret_reads() {
        use usagi_core::domain::settings::{EnvLimitError, MAX_ENV_BINDINGS};

        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        write_global(
            data.path(),
            (0..MAX_ENV_BINDINGS)
                .map(|index| (format!("GLOBAL_{index}"), "literal".to_owned()))
                .collect(),
        );
        write_workspace(
            workspace.path(),
            bindings(&[("WORKSPACE_SECRET", "op://Private/Secret/value")]),
        );
        let environment = UserEnvironment::new(data.path().to_path_buf(), CountingResolver::new());

        assert_eq!(
            environment.resolved(workspace.path()),
            Err(UserEnvironmentError::Limits(EnvLimitError::TooManyBindings))
        );
        assert!(environment.resolver.reads().is_empty());
    }

    #[test]
    fn workspace_launcher_control_bindings_are_rejected_before_secret_resolution() {
        let symlink_target = tempfile::tempdir().unwrap();
        let symlink = symlink_target.path().with_extension("alias");
        #[cfg(unix)]
        std::os::unix::fs::symlink(symlink_target.path(), &symlink).unwrap();
        let symlink_value = symlink.to_string_lossy();
        let cases = [
            ("PATH", "/workspace/fake-bin"),
            ("TMPDIR", "/"),
            ("HOME", "/"),
            ("USAGI_CLAUDE_SANDBOX_PASSTHROUGH", "1"),
            ("TMPDIR", symlink_value.as_ref()),
        ];
        for (name, value) in cases {
            let data = tempfile::tempdir().unwrap();
            let workspace = tempfile::tempdir().unwrap();
            write_global(
                data.path(),
                bindings(&[("CREDENTIAL", "op://Private/Credential/value")]),
            );
            write_workspace(workspace.path(), bindings(&[(name, value)]));
            let environment =
                UserEnvironment::new(data.path().to_path_buf(), CountingResolver::new());

            assert_eq!(
                environment.resolved(workspace.path()),
                Err(UserEnvironmentError::ReservedLauncherVariable),
                "{name} must fail admission"
            );
            assert!(
                environment.resolver.reads().is_empty(),
                "{name} must fail before resolving a credential"
            );
        }
        #[cfg(unix)]
        std::fs::remove_file(symlink).unwrap();
    }

    #[test]
    fn a_settings_mutation_makes_the_second_dispatch_effect_free() {
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        write_workspace(
            workspace.path(),
            bindings(&[("CREDENTIAL", "op://Private/First/value")]),
        );
        let environment = UserEnvironment::new(data.path().to_path_buf(), CountingResolver::new());
        assert!(
            environment
                .resolved(workspace.path())
                .unwrap()
                .contains_key("CREDENTIAL")
        );

        write_workspace(
            workspace.path(),
            bindings(&[
                ("PATH", "/workspace/fake-bin"),
                ("NEXT_CREDENTIAL", "op://Private/Second/value"),
            ]),
        );
        assert_eq!(
            environment.resolved(workspace.path()),
            Err(UserEnvironmentError::ReservedLauncherVariable)
        );
        assert_eq!(
            environment.resolver.reads(),
            ["op://Private/First/value"],
            "the rejected second dispatch must not resolve its credential"
        );
    }

    #[test]
    fn over_limit_global_or_workspace_load_is_a_safe_admission_error() {
        use usagi_core::domain::settings::{EnvLimitError, MAX_ENV_BINDINGS};

        let oversized = (0..=MAX_ENV_BINDINGS)
            .map(|index| (format!("VALUE_{index}"), "literal".to_owned()))
            .collect::<EnvBindings>();

        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(
            data.path().join("settings.json"),
            serde_json::to_vec(&Settings {
                env: oversized.clone(),
                ..Settings::default()
            })
            .unwrap(),
        )
        .unwrap();
        let global = UserEnvironment::new(data.path().to_path_buf(), CountingResolver::new());
        assert_eq!(
            global.resolved(workspace.path()),
            Err(UserEnvironmentError::Limits(EnvLimitError::TooManyBindings))
        );
        assert!(global.resolver.reads().is_empty());

        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let local = WorkspaceSettingsStore::new(workspace.path());
        std::fs::create_dir_all(local.path().parent().unwrap()).unwrap();
        std::fs::write(
            local.path(),
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "env": oversized,
            }))
            .unwrap(),
        )
        .unwrap();
        let workspace_env =
            UserEnvironment::new(data.path().to_path_buf(), CountingResolver::new());
        assert_eq!(
            workspace_env.resolved(workspace.path()),
            Err(UserEnvironmentError::Limits(EnvLimitError::TooManyBindings))
        );
        assert!(workspace_env.resolver.reads().is_empty());
    }

    #[test]
    fn typed_names_and_allowlist_skip_what_cannot_name_a_variable() {
        let values = BTreeMap::from([
            ("GH_TOKEN".to_owned(), "secret".to_owned()),
            // Only reachable by a caller bypassing the settings validation.
            ("not a name".to_owned(), "ignored".to_owned()),
        ]);
        assert_eq!(
            allowlist(&values)
                .iter()
                .map(|name| name.as_str().to_owned())
                .collect::<Vec<_>>(),
            ["GH_TOKEN"]
        );
        assert_eq!(
            typed(&values)
                .into_iter()
                .map(|(name, value)| (name.as_str().to_owned(), value))
                .collect::<Vec<_>>(),
            [("GH_TOKEN".to_owned(), "secret".to_owned())]
        );
    }
}

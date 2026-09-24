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
//! 1Password CLI. A resolved secret is cached per **secret reference and
//! credential** rather than per workspace, because one daemon owns every
//! workspace it adopted ([5. daemon](../../document/05-daemon.md#tenant-registry)):
//! a reference configured globally is read once for all of them, so a 1Password
//! desktop approval is asked once instead of once per workspace. Editing a
//! reference resolves that binding again, changing the `op read` credential
//! resolves every reference again, and a pane already running keeps the
//! environment it started with.
//!
//! A binding that cannot be resolved is dropped and logged: a locked vault
//! leaves one variable unset instead of making a pane impossible to open.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sha2::{Digest as _, Sha256};
use usagi_core::domain::agent::EnvironmentVariableName;
use usagi_core::domain::settings::{
    EnvBindings, EnvLimitError, MAX_SECRET_REFERENCES, Settings, is_secret_reference,
    valid_bindings, validate_env_limits,
};
use usagi_core::infrastructure::env_resolver::{
    OpCli, resolve_parallel_with_service_account_token,
};
use usagi_core::infrastructure::error_log::ErrorLog;
use usagi_core::infrastructure::store::settings::WorkspaceSettingsStore;
use usagi_core::infrastructure::store::workspace::Storage;
use usagi_core::usecase::env::SecretResolver;

const OP_SERVICE_ACCOUNT_TOKEN: &str = "OP_SERVICE_ACCOUNT_TOKEN";

struct ConfiguredEnvironment {
    bindings: EnvBindings,
    service_account_token: Option<String>,
}

/// A resolved secret is reusable only for the credential that read it: the
/// [identity](credential_identity) of that credential, then the reference.
type SecretCacheKey = (String, String);

/// How many resolved secrets one daemon keeps in memory.
///
/// One configuration holds at most [`MAX_SECRET_REFERENCES`] references, so this
/// is eight fully loaded configurations' worth. A reference that leaves the
/// configuration — an edited binding, a rotated credential — is not evicted on
/// its own, so this bound is what stops superseded values from accumulating for
/// the life of the daemon. Overflowing clears the cache instead of evicting one
/// entry: the next launch resolves again, which is correct, only slower.
const MAX_CACHED_SECRETS: usize = MAX_SECRET_REFERENCES * 8;

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

/// Variables a **workspace** may not bind, because they decide what a managed
/// launch *is* rather than what it can see.
///
/// `.usagi/settings.json` travels with a repository, so a binding here would let
/// a checkout redirect an agent CLI: `PATH`/`HOME`/`TMPDIR`/`CODEX_HOME` at the
/// filesystem it uses, and the gateway variables at the endpoint it talks to.
/// The latter matter even for providers usagi does not point anywhere: binding
/// `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN` would send the user's Claude
/// session — prompts, file contents, credentials in flight — to a server the
/// repository chose. usagi owns them per provider
/// ([`DefaultModel::gateway_environment`]), so a workspace binding could only
/// ever be an override of that decision.
const WORKSPACE_AGENT_CONTROL_VARIABLES: [&str; 15] = [
    "PATH",
    "TMPDIR",
    "HOME",
    "CODEX_HOME",
    "CLAUDE_CONFIG_DIR",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    "CLAUDE_CODE_SUBAGENT_MODEL",
    // The provider API key usagi injects. Reserving it keeps one machine-level
    // value behind the readiness probe and the launch: the probe has no
    // workspace, so a workspace-scoped key would let a launch be admitted — or
    // refused — on a credential that is not the one it would use.
    "SAKANA_API_KEY",
    usagi_core::usecase::claude_sandbox::PASSTHROUGH_ENVIRONMENT_VARIABLE,
];

/// The configured environment of a workspace, resolving each secret once for
/// every workspace this daemon serves.
pub struct UserEnvironment<R = OpCli> {
    global: Storage,
    resolver: R,
    /// Per `(credential identity, secret reference)`: the value `op read`
    /// returned. Keyed by the reference rather than by the workspace, so a
    /// reference configured globally costs one read no matter how many
    /// workspaces bind it. A failed read is not stored, so a locked vault is
    /// retried on the next launch instead of being fixed for the life of the
    /// daemon.
    ///
    /// The lock is held across resolution deliberately: two launches racing on
    /// the same reference then wait for one `op read` instead of asking
    /// 1Password for two approvals.
    secrets: Mutex<BTreeMap<SecretCacheKey, String>>,
}

impl<R: SecretResolver + Sync> UserEnvironment<R> {
    /// Read settings from `data_dir` and resolve secrets through `resolver`.
    pub fn new(data_dir: PathBuf, resolver: R) -> Self {
        Self {
            global: Storage::new(data_dir),
            resolver,
            secrets: Mutex::new(BTreeMap::new()),
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
            .any(|name| WORKSPACE_AGENT_CONTROL_VARIABLES.contains(&name.as_str()))
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
        let credential = credential_identity(configured.service_account_token.as_deref());
        let mut secrets = self
            .secrets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut values = BTreeMap::new();
        let mut pending = EnvBindings::new();
        for (name, value) in valid_bindings(&configured.bindings) {
            let cached = is_secret_reference(value)
                .then(|| secrets.get(&(credential.clone(), value.to_owned())))
                .flatten();
            if let Some(secret) = cached {
                values.insert(name.to_owned(), secret.clone());
            } else {
                pending.insert(name.to_owned(), value.to_owned());
            }
        }
        let resolved = resolve_parallel_with_service_account_token(
            &pending,
            &self.resolver,
            configured.service_account_token.as_deref(),
        )
        .expect("a subset of the validated bindings preserves the env limits");
        for failure in &resolved.failures {
            ErrorLog::record(&format!(
                "could not resolve environment variable {} from {}: {}",
                failure.name, failure.reference, failure.error
            ));
        }
        for (name, value) in resolved.values {
            if let Some(reference) = pending
                .get(&name)
                .filter(|reference| is_secret_reference(reference))
            {
                if secrets.len() >= MAX_CACHED_SECRETS {
                    secrets.clear();
                }
                secrets.insert((credential.clone(), reference.clone()), value.clone());
            }
            values.insert(name, value);
        }
        Ok(values)
    }
}

/// A stable identity for the credential `op read` will authenticate with, so a
/// cached secret is never handed to a launch that authenticates as someone else.
///
/// The token itself is not the key. The cache outlives the launch that filled
/// it, and a digest tells two credentials apart just as well as the credential
/// does.
fn credential_identity(service_account_token: Option<&str>) -> String {
    let mut digest = Sha256::new();
    digest.update(b"usagi-op-credential-v1");
    match service_account_token {
        Some(token) => {
            digest.update([1u8]);
            digest.update((token.len() as u64).to_be_bytes());
            digest.update(token.as_bytes());
        }
        None => digest.update([0u8]),
    }
    let digest = digest.finalize();
    let mut identity = String::with_capacity(32);
    for byte in &digest[..16] {
        write!(&mut identity, "{byte:02x}").expect("writing to a String cannot fail");
    }
    identity
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
    use super::{
        MAX_CACHED_SECRETS, MAX_SECRET_REFERENCES, UserEnvironment, UserEnvironmentError,
        WORKSPACE_AGENT_CONTROL_VARIABLES, allowlist, typed,
    };
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

        // Editing an unrelated binding leaves the resolved secret in place.
        write_workspace(workspace.path(), bindings(&[("RUST_LOG", "trace")]));
        assert_eq!(
            environment
                .resolved(workspace.path())
                .unwrap()
                .get("RUST_LOG"),
            Some(&"trace".to_owned())
        );
        assert_eq!(
            environment.resolver.reads(),
            ["op://Private/GitHub/token"],
            "only the edited binding is affected"
        );

        // Editing the reference itself resolves the new one.
        write_global(
            data.path(),
            bindings(&[("GH_TOKEN", "op://Private/GitHub/rotated")]),
        );
        assert_eq!(
            environment
                .resolved(workspace.path())
                .unwrap()
                .get("GH_TOKEN"),
            Some(&"secret:op://Private/GitHub/rotated".to_owned())
        );
        assert_eq!(
            environment.resolver.reads(),
            ["op://Private/GitHub/token", "op://Private/GitHub/rotated"]
        );
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

    /// The reason this cache is keyed by the reference rather than by the
    /// workspace: a global `op://` binding is the same secret for every
    /// workspace, and reading it again asks the user for a 1Password approval
    /// they have already given.
    #[test]
    fn a_shared_reference_is_read_once_for_every_workspace() {
        let data = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        write_global(
            data.path(),
            bindings(&[("SHARED", "op://Private/Shared/token")]),
        );
        write_workspace(first.path(), bindings(&[("WHICH", "first")]));
        write_workspace(second.path(), bindings(&[("WHICH", "second")]));
        let environment = UserEnvironment::new(data.path().to_path_buf(), CountingResolver::new());

        let shared = "secret:op://Private/Shared/token".to_owned();
        assert_eq!(
            environment.resolved(first.path()).unwrap(),
            BTreeMap::from([
                ("SHARED".to_owned(), shared.clone()),
                ("WHICH".to_owned(), "first".to_owned()),
            ])
        );
        assert_eq!(
            environment.resolved(second.path()).unwrap(),
            BTreeMap::from([
                ("SHARED".to_owned(), shared),
                // A workspace's own literal is still its own.
                ("WHICH".to_owned(), "second".to_owned()),
            ])
        );
        assert_eq!(
            environment.resolver.reads(),
            ["op://Private/Shared/token"],
            "the second workspace must not ask 1Password again"
        );
    }

    #[test]
    fn a_failed_read_is_retried_on_the_next_launch() {
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        write_global(
            data.path(),
            bindings(&[("LOCKED", "op://Private/Locked/token")]),
        );
        let environment = UserEnvironment::new(data.path().to_path_buf(), CountingResolver::new());

        assert!(environment.resolved(workspace.path()).unwrap().is_empty());
        assert!(environment.resolved(workspace.path()).unwrap().is_empty());
        assert_eq!(
            environment.resolver.reads(),
            ["op://Private/Locked/token", "op://Private/Locked/token"],
            "a vault unlocked later must still be able to resolve"
        );
    }

    /// A superseded reference is never evicted on its own, so this bound is the
    /// only thing keeping a long-lived daemon's cache from growing with every
    /// edit.
    #[test]
    fn the_secret_cache_is_bounded() {
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let environment = UserEnvironment::new(data.path().to_path_buf(), CountingResolver::new());

        for generation in 0..MAX_CACHED_SECRETS / MAX_SECRET_REFERENCES {
            write_global(
                data.path(),
                (0..MAX_SECRET_REFERENCES)
                    .map(|index| {
                        (
                            format!("SECRET_{index}"),
                            format!("op://Private/{generation}/{index}"),
                        )
                    })
                    .collect(),
            );
            environment.resolved(workspace.path()).unwrap();
        }
        assert_eq!(
            environment.secrets.lock().unwrap().len(),
            MAX_CACHED_SECRETS
        );

        write_global(
            data.path(),
            bindings(&[("EXTRA", "op://Private/extra/token")]),
        );
        environment.resolved(workspace.path()).unwrap();
        assert_eq!(
            environment.secrets.lock().unwrap().len(),
            1,
            "overflowing starts over rather than growing without bound"
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
            ("CODEX_HOME", "/workspace/.codex"),
            ("USAGI_CLAUDE_SANDBOX_PASSTHROUGH", "1"),
            ("TMPDIR", symlink_value.as_ref()),
            // A checked-in binding must not be able to point a managed Claude
            // launch at another endpoint, or hand it another account's token.
            ("ANTHROPIC_BASE_URL", "https://attacker.example"),
            ("ANTHROPIC_AUTH_TOKEN", "stolen"),
            ("ANTHROPIC_API_KEY", "stolen"),
            ("ANTHROPIC_DEFAULT_OPUS_MODEL", "attacker-model"),
            ("ANTHROPIC_DEFAULT_SONNET_MODEL", "attacker-model"),
            ("ANTHROPIC_DEFAULT_HAIKU_MODEL", "attacker-model"),
            ("ANTHROPIC_DEFAULT_FABLE_MODEL", "attacker-model"),
            ("CLAUDE_CODE_SUBAGENT_MODEL", "attacker-model"),
            // Nor at another provider's state directory.
            ("CLAUDE_CONFIG_DIR", "/workspace/.claude"),
            // The provider key is machine-level so the probe and the launch
            // cannot disagree about which credential is configured.
            ("SAKANA_API_KEY", "workspace-key"),
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

    /// Every name usagi itself injects to *define* a provider has to be reserved
    /// from workspace bindings, and the set is derived from the vocabulary
    /// rather than retyped here: adding a gateway variable without reserving it
    /// would otherwise ship a name a checked-in `.usagi/settings.json` can
    /// override, silently changing which model — or which account — a managed
    /// launch uses.
    #[test]
    fn every_variable_usagi_injects_for_a_provider_is_reserved_from_workspaces() {
        for model in usagi_core::domain::settings::DefaultModel::ALL {
            let injected = model
                .gateway_environment()
                .iter()
                .map(|(name, _)| *name)
                .chain(model.state_directory_env())
                .chain(
                    model
                        .credential_binding()
                        .into_iter()
                        .flat_map(|(source, target)| [source, target]),
                );
            for name in injected {
                assert!(
                    WORKSPACE_AGENT_CONTROL_VARIABLES.contains(&name),
                    "{model:?} injects {name}, so a workspace must not be able to bind it"
                );
            }
        }
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

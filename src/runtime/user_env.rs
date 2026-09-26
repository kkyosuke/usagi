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
//! 1Password CLI. A **globally** configured reference is resolved once for every
//! workspace this daemon serves, because one daemon owns every workspace it
//! adopted ([5. daemon](../../document/05-daemon.md#tenant-registry)) — so a
//! 1Password approval is asked once instead of once per workspace. A reference a
//! *workspace* configured is cached for that workspace alone: `.usagi/settings.json`
//! travels with a repository, and a checkout naming a reference has to face
//! 1Password itself rather than collect an approval the user gave for their own
//! binding.
//!
//! Editing a reference resolves that binding again and changing
//! `OP_SERVICE_ACCOUNT_TOKEN` resolves every reference again, but a secret
//! *rotated in 1Password* behind an unchanged reference is only picked up by a
//! new daemon — as is a different `op signin` account, which this cache cannot
//! see. A pane already running keeps the environment it started with.
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
    /// The names the **workspace** bound, which decide the [scope](SecretCacheKey)
    /// their references are cached under.
    workspace_scoped: BTreeSet<String>,
    service_account_token: Option<String>,
}

impl ConfiguredEnvironment {
    /// Where a resolved `reference` may be reused from.
    ///
    /// A binding the workspace declared is scoped to that workspace, so a
    /// checked-in `.usagi/settings.json` cannot read a value the user's own
    /// global binding already had approved. A global binding is scoped to the
    /// daemon, which is the whole point of the cache.
    fn cache_key(
        &self,
        credential: &str,
        workspace_root: &Path,
        name: &str,
        reference: &str,
    ) -> SecretCacheKey {
        let scope = self
            .workspace_scoped
            .contains(name)
            .then(|| workspace_root.to_path_buf());
        (credential.to_owned(), scope, reference.to_owned())
    }
}

/// What makes two resolutions of the same reference interchangeable: the
/// [identity](credential_identity) of the credential that read it, the workspace
/// that configured it (`None` for a global binding, shared by every workspace),
/// and the reference itself.
type SecretCacheKey = (String, Option<PathBuf>, String);

/// How many resolved secrets one daemon keeps in memory.
///
/// One configuration holds at most [`MAX_SECRET_REFERENCES`] references, so this
/// is eight fully loaded configurations' worth. A reference that leaves the
/// configuration — an edited binding, a rotated credential — is not evicted on
/// its own, so this bound is what stops superseded values from accumulating for
/// the life of the daemon.
const MAX_CACHED_SECRETS: usize = MAX_SECRET_REFERENCES * 8;

/// The resolved secrets of one daemon, evicted least-recently-used first.
///
/// Eviction is per entry rather than a wholesale clear: a working set larger
/// than the bound would otherwise drop everything on every launch and ask
/// 1Password for each reference again — the very cost this cache exists to
/// remove, reached silently.
#[derive(Default)]
struct SecretCache {
    /// Per key: when the entry was last used, and the value `op read` returned.
    entries: BTreeMap<SecretCacheKey, (u64, String)>,
    /// Monotonic use counter. A launch resolving 32 references every second
    /// would take more than ten billion years to exhaust it.
    next_use: u64,
}

impl SecretCache {
    /// The cached value for `key`, counted as a use so it survives eviction.
    fn get(&mut self, key: &SecretCacheKey) -> Option<String> {
        let use_count = self.next_use;
        let entry = self.entries.get_mut(key)?;
        entry.0 = use_count;
        self.next_use += 1;
        Some(entry.1.clone())
    }

    /// Store `value`, evicting the least recently used entry when full.
    fn insert(&mut self, key: SecretCacheKey, value: String) {
        if self.entries.len() >= MAX_CACHED_SECRETS {
            let evicted = self
                .entries
                .iter()
                .min_by_key(|(_, (use_count, _))| *use_count)
                .map(|(key, _)| key.clone())
                .expect("a cache at its bound holds at least one entry");
            self.entries.remove(&evicted);
        }
        let use_count = self.next_use;
        self.next_use += 1;
        self.entries.insert(key, (use_count, value));
    }
}

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
/// filesystem it uses, and the gateway variables at the endpoint it talks to:
/// binding `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN` would send the user's
/// Claude session — prompts, file contents, credentials in flight — to a server
/// the repository chose.
const WORKSPACE_AGENT_CONTROL_VARIABLES: [&str; 14] = [
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
    usagi_core::usecase::claude_sandbox::PASSTHROUGH_ENVIRONMENT_VARIABLE,
];

/// The configured environment of a workspace, resolving each secret once for
/// every workspace this daemon serves.
pub struct UserEnvironment<R = OpCli> {
    global: Storage,
    resolver: R,
    /// The secrets already read, keyed by [`SecretCacheKey`] so a global
    /// reference costs one read no matter how many workspaces this daemon
    /// serves. A failed read is not stored, so a locked vault is retried on the
    /// next launch instead of being fixed for the life of the daemon.
    ///
    /// The lock is held across resolution deliberately: two launches racing on
    /// the same reference then wait for one `op read` instead of asking
    /// 1Password for two approvals.
    secrets: Mutex<SecretCache>,
}

impl<R: SecretResolver + Sync> UserEnvironment<R> {
    /// Read settings from `data_dir` and resolve secrets through `resolver`.
    pub fn new(data_dir: PathBuf, resolver: R) -> Self {
        Self {
            global: Storage::new(data_dir),
            resolver,
            secrets: Mutex::new(SecretCache::default()),
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
        let workspace_scoped = local
            .env
            .keys()
            .map(|name| name.trim().to_owned())
            .collect();
        let mut bindings = global.with_local(&local).env;
        validate_env_limits(&bindings)?;
        let service_account_token = bindings.remove(OP_SERVICE_ACCOUNT_TOKEN);
        Ok(ConfiguredEnvironment {
            bindings,
            workspace_scoped,
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
                .then(|| {
                    secrets.get(&configured.cache_key(&credential, workspace_root, name, value))
                })
                .flatten();
            if let Some(secret) = cached {
                values.insert(name.to_owned(), secret);
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
                let key = configured.cache_key(&credential, workspace_root, &name, reference);
                secrets.insert(key, value.clone());
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
        allowlist, credential_identity, typed,
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
            match (reference.contains("Locked"), service_account_token) {
                (true, _) => Err("op is locked".to_owned()),
                // The credential is part of the value so a test can tell which
                // one a cached secret was read with.
                (false, Some(token)) => Ok(format!("secret:{token}:{reference}")),
                (false, None) => Ok(format!("secret:{reference}")),
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
        assert_eq!(
            values["GH_TOKEN"],
            "secret:global-token:op://Private/GitHub/token"
        );
        assert!(!values.contains_key("OP_SERVICE_ACCOUNT_TOKEN"));
        assert_eq!(
            environment.resolver.service_account_tokens(),
            [Some("global-token".to_owned())]
        );

        write_workspace(
            workspace.path(),
            bindings(&[("OP_SERVICE_ACCOUNT_TOKEN", "workspace-token")]),
        );
        assert_eq!(
            environment.resolved(workspace.path()).unwrap()["GH_TOKEN"],
            "secret:workspace-token:op://Private/GitHub/token",
            "a value read under another credential must not be reused"
        );
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

        // Going back to a credential that already read this reference reuses
        // that credential's value rather than the newer one.
        write_global(
            data.path(),
            bindings(&[
                ("GH_TOKEN", "op://Private/GitHub/token"),
                ("OP_SERVICE_ACCOUNT_TOKEN", "global-token"),
            ]),
        );
        write_workspace(workspace.path(), EnvBindings::new());
        assert_eq!(
            environment.resolved(workspace.path()).unwrap()["GH_TOKEN"],
            "secret:global-token:op://Private/GitHub/token"
        );
        assert_eq!(
            environment.resolver.service_account_tokens().len(),
            3,
            "the first credential's value is still cached"
        );
    }

    /// `.usagi/settings.json` travels with a repository, so a reference the
    /// *workspace* names must face 1Password itself instead of collecting an
    /// approval the user gave for their own global binding.
    #[test]
    fn a_workspace_scoped_reference_does_not_read_the_global_cache() {
        let data = tempfile::tempdir().unwrap();
        let mine = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        write_global(
            data.path(),
            bindings(&[("GH_TOKEN", "op://Private/GitHub/token")]),
        );
        write_workspace(
            checkout.path(),
            bindings(&[("EXFIL", "op://Private/GitHub/token")]),
        );
        let environment = UserEnvironment::new(data.path().to_path_buf(), CountingResolver::new());

        environment.resolved(mine.path()).unwrap();
        environment.resolved(checkout.path()).unwrap();
        assert_eq!(
            environment.resolver.reads(),
            ["op://Private/GitHub/token", "op://Private/GitHub/token"],
            "the checkout's own binding must still be read"
        );

        // It is cached for that workspace, though, so relaunching it is free.
        environment.resolved(checkout.path()).unwrap();
        assert_eq!(environment.resolver.reads().len(), 2);

        // And another workspace naming it in its own settings reads again.
        let other = tempfile::tempdir().unwrap();
        write_workspace(
            other.path(),
            bindings(&[("EXFIL", "op://Private/GitHub/token")]),
        );
        environment.resolved(other.path()).unwrap();
        assert_eq!(environment.resolver.reads().len(), 3);
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

    /// A superseded reference is never evicted on its own, so the bound is what
    /// keeps a long-lived daemon's cache from growing with every edit. It has to
    /// evict rather than clear: a working set larger than the bound would
    /// otherwise drop everything on every launch and re-ask 1Password for every
    /// reference — worse than not caching at all.
    #[test]
    fn a_full_secret_cache_evicts_the_least_recently_used_entry() {
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let environment = UserEnvironment::new(data.path().to_path_buf(), CountingResolver::new());
        let generations = MAX_CACHED_SECRETS / MAX_SECRET_REFERENCES;

        for generation in 0..generations {
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
        let filled = environment.resolver.reads().len();
        assert_eq!(
            environment.secrets.lock().unwrap().entries.len(),
            MAX_CACHED_SECRETS
        );

        // Relaunching the current configuration reads nothing and counts every
        // one of its references as used.
        environment.resolved(workspace.path()).unwrap();
        assert_eq!(environment.resolver.reads().len(), filled);

        write_global(
            data.path(),
            bindings(&[("EXTRA", "op://Private/extra/token")]),
        );
        environment.resolved(workspace.path()).unwrap();
        let secrets = environment.secrets.lock().unwrap();
        assert_eq!(
            secrets.entries.len(),
            MAX_CACHED_SECRETS,
            "the cache stays at its bound"
        );
        let credential = credential_identity(None);
        assert!(
            !secrets.entries.contains_key(&(
                credential.clone(),
                None,
                "op://Private/0/0".to_owned()
            )),
            "the oldest entry is the one evicted"
        );
        assert!(
            secrets.entries.contains_key(&(
                credential,
                None,
                format!("op://Private/{}/0", generations - 1)
            )),
            "a reference the last launch used stays cached"
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

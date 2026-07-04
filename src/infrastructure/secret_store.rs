//! A small port over the operating system's native secret store, plus its real
//! implementation.
//!
//! `usagi op login` keeps the 1Password service-account token out of usagi's
//! (syncable, plaintext) `settings.json` and in the OS-native secret store
//! instead — Apple Keychain on macOS, the Windows Credential Manager on Windows,
//! and the Linux kernel keyutils store on Linux. When a pane launches, the
//! [`env_resolver`](crate::infrastructure::env_resolver) reads the stored token
//! back and passes it to `op read` so 1Password references resolve even in a
//! non-interactive session.
//!
//! The [`SecretStore`] trait is the seam the `usagi op login` use case is tested
//! against with an in-memory fake; [`SystemSecretStore`] is the real OS IO, so
//! this file is excluded from coverage (see `scripts/coverage.sh`).

/// The keychain entry name under which usagi stores the 1Password service
/// account token. Stable so `usagi op login` and the env resolver address the
/// same entry.
pub const OP_SERVICE_ACCOUNT_TOKEN_KEY: &str = "op_service_account_token";

/// The keyring "service" namespace usagi stores its credentials under. The entry
/// "user" is the secret's key (e.g. [`OP_SERVICE_ACCOUNT_TOKEN_KEY`]).
pub const KEYRING_SERVICE: &str = "usagi";

/// Store a single named secret in the OS secret store.
///
/// Abstracted so the `usagi op login` use case is unit-tested against an
/// in-memory fake instead of the real keychain.
pub trait SecretStore {
    /// Store (or replace) the secret for `key`.
    fn set(&self, key: &str, value: &str) -> Result<(), String>;
}

/// The production [`SecretStore`] backed by the OS-native secret store via the
/// cross-platform [`keyring`] crate. Kept here as thin real IO (excluded from
/// coverage); the use cases are tested against an injected fake store.
pub struct SystemSecretStore;

impl SystemSecretStore {
    /// The stored secret for `key`, or `None` when no entry exists or the store
    /// cannot be read. Best-effort: the env resolver falls back to `op`'s own
    /// ambient session when the token is absent.
    pub fn get(&self, key: &str) -> Option<String> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, key).ok()?;
        entry.get_password().ok()
    }
}

impl SecretStore for SystemSecretStore {
    fn set(&self, key: &str, value: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, key)
            .map_err(|e| format!("opening keychain entry: {e}"))?;
        entry
            .set_password(value)
            .map_err(|e| format!("writing keychain entry: {e}"))
    }
}

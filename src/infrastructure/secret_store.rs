//! A small port over the operating system's secret store (macOS Keychain and the
//! Linux Secret Service / kernel keyring where available).
//!
//! usagi keeps the 1Password service-account token out of its (syncable,
//! plaintext) `settings.json` and in the OS-native secret store instead. The
//! actual store is reached by shelling out to the platform's secret tool
//! (`security` on macOS, `secret-tool` on Linux) — so this brings in no new
//! dependency, matching how usagi already drives `op`, `git` and `ollama`. That
//! real-IO implementation lives at the composition root (`main.rs`,
//! coverage-excluded); everything that *uses* a store takes this trait so it can
//! be unit-tested with an in-memory fake.

/// The keychain entry name under which usagi stores the 1Password service
/// account token. Stable so `op login` / `op logout` / the `op-mcp` server all
/// address the same entry.
pub const OP_SERVICE_ACCOUNT_TOKEN_KEY: &str = "op_service_account_token";

/// Read/write/delete a single named secret in the OS secret store.
///
/// All three methods map a *missing* entry to a non-error outcome
/// ([`get`](SecretStore::get) returns `Ok(None)`, [`delete`](SecretStore::delete)
/// returns `Ok(())`), so callers distinguish "absent" from "the store failed".
pub trait SecretStore {
    /// The stored secret for `key`, or `Ok(None)` when no entry exists.
    fn get(&self, key: &str) -> Result<Option<String>, String>;

    /// Store (or replace) the secret for `key`.
    fn set(&self, key: &str, value: &str) -> Result<(), String>;

    /// Remove the secret for `key`. Deleting an absent entry is not an error.
    fn delete(&self, key: &str) -> Result<(), String>;
}

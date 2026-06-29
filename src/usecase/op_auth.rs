//! Use cases for managing the 1Password service-account token used by
//! `usagi op-mcp`.
//!
//! The token itself lives in the OS secret store, never in `settings.json`.
//! Settings only carry the non-secret `op_mcp.enabled` flag so launched agents
//! know whether to wire the `usagi-op` MCP server.

use crate::domain::settings::Settings;
use crate::infrastructure::secret_store::{SecretStore, OP_SERVICE_ACCOUNT_TOKEN_KEY};

/// Store `token` in the OS secret store and enable `op-mcp` in settings.
pub fn login(
    store: &dyn SecretStore,
    mut settings: Settings,
    token: &str,
) -> Result<Settings, String> {
    let token = token.trim();
    if token.is_empty() {
        return Err("token must not be empty".to_string());
    }
    store.set(OP_SERVICE_ACCOUNT_TOKEN_KEY, token)?;
    settings.op_mcp.enabled = true;
    Ok(settings)
}

/// Remove the stored token and disable `op-mcp` in settings.
pub fn logout(store: &dyn SecretStore, mut settings: Settings) -> Result<Settings, String> {
    store.delete(OP_SERVICE_ACCOUNT_TOKEN_KEY)?;
    settings.op_mcp.enabled = false;
    Ok(settings)
}

/// Report whether `op-mcp` is enabled and whether a token is present in the OS
/// secret store.
pub fn status(store: &dyn SecretStore, settings: &Settings) -> Result<OpAuthStatus, String> {
    Ok(OpAuthStatus {
        enabled: settings.op_mcp.enabled,
        token_present: store.get(OP_SERVICE_ACCOUNT_TOKEN_KEY)?.is_some(),
    })
}

/// The non-secret status shown by `usagi op status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpAuthStatus {
    /// Whether launched agents are configured to wire `usagi-op`.
    pub enabled: bool,
    /// Whether a token entry exists in the OS secret store.
    pub token_present: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct FakeStore {
        value: RefCell<Option<String>>,
        fail: RefCell<Option<String>>,
    }

    impl FakeStore {
        fn with_value(value: &str) -> Self {
            Self {
                value: RefCell::new(Some(value.to_string())),
                fail: RefCell::new(None),
            }
        }

        fn failing(message: &str) -> Self {
            Self {
                value: RefCell::new(None),
                fail: RefCell::new(Some(message.to_string())),
            }
        }

        fn maybe_fail(&self) -> Result<(), String> {
            match self.fail.borrow().clone() {
                Some(message) => Err(message),
                None => Ok(()),
            }
        }
    }

    impl SecretStore for FakeStore {
        fn get(&self, _key: &str) -> Result<Option<String>, String> {
            self.maybe_fail()?;
            Ok(self.value.borrow().clone())
        }

        fn set(&self, _key: &str, value: &str) -> Result<(), String> {
            self.maybe_fail()?;
            self.value.replace(Some(value.to_string()));
            Ok(())
        }

        fn delete(&self, _key: &str) -> Result<(), String> {
            self.maybe_fail()?;
            self.value.replace(None);
            Ok(())
        }
    }

    #[test]
    fn login_stores_a_trimmed_token_and_enables_settings() {
        let store = FakeStore::default();
        let settings = login(&store, Settings::default(), "  ops_abc  ").unwrap();
        assert!(settings.op_mcp.enabled);
        assert_eq!(
            store.get(OP_SERVICE_ACCOUNT_TOKEN_KEY).unwrap().as_deref(),
            Some("ops_abc")
        );
    }

    #[test]
    fn login_rejects_blank_tokens_without_enabling() {
        let store = FakeStore::default();
        let err = login(&store, Settings::default(), "  ").unwrap_err();
        assert!(err.contains("must not be empty"));
        assert_eq!(store.get(OP_SERVICE_ACCOUNT_TOKEN_KEY).unwrap(), None);
    }

    #[test]
    fn logout_deletes_the_token_and_disables_settings() {
        let store = FakeStore::with_value("ops_abc");
        let mut settings = Settings::default();
        settings.op_mcp.enabled = true;
        let settings = logout(&store, settings).unwrap();
        assert!(!settings.op_mcp.enabled);
        assert_eq!(store.get(OP_SERVICE_ACCOUNT_TOKEN_KEY).unwrap(), None);
    }

    #[test]
    fn status_reports_settings_and_secret_presence() {
        let store = FakeStore::with_value("ops_abc");
        let mut settings = Settings::default();
        settings.op_mcp.enabled = true;
        assert_eq!(
            status(&store, &settings).unwrap(),
            OpAuthStatus {
                enabled: true,
                token_present: true
            }
        );
    }

    #[test]
    fn store_errors_are_propagated() {
        let store = FakeStore::failing("keychain unavailable");
        assert!(login(&store, Settings::default(), "ops").is_err());
        assert!(logout(&store, Settings::default()).is_err());
        assert!(status(&store, &Settings::default()).is_err());
    }
}

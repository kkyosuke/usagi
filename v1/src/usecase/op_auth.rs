//! Use case for `usagi op login`: store the 1Password service-account token used
//! to resolve workspace `op://` references non-interactively.
//!
//! The token lives in the OS secret store, never in `settings.json`. The env
//! resolver reads it back when a pane launches (see
//! [`env_resolver`](crate::infrastructure::env_resolver)).

use crate::infrastructure::secret_store::{SecretStore, OP_SERVICE_ACCOUNT_TOKEN_KEY};

/// Store `token` in the OS secret store.
///
/// The token is trimmed; a blank token is rejected so `login` never persists an
/// empty credential.
pub fn login(store: &dyn SecretStore, token: &str) -> Result<(), String> {
    let token = token.trim();
    if token.is_empty() {
        return Err("token must not be empty".to_string());
    }
    store.set(OP_SERVICE_ACCOUNT_TOKEN_KEY, token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct FakeStore {
        value: RefCell<Option<String>>,
        fail: bool,
    }

    impl SecretStore for FakeStore {
        fn set(&self, _key: &str, value: &str) -> Result<(), String> {
            if self.fail {
                return Err("keychain unavailable".to_string());
            }
            self.value.replace(Some(value.to_string()));
            Ok(())
        }
    }

    #[test]
    fn login_stores_a_trimmed_token() {
        let store = FakeStore::default();
        login(&store, "  ops_abc  ").unwrap();
        assert_eq!(store.value.borrow().as_deref(), Some("ops_abc"));
    }

    #[test]
    fn login_rejects_blank_tokens() {
        let store = FakeStore::default();
        let err = login(&store, "  ").unwrap_err();
        assert!(err.contains("must not be empty"));
        assert_eq!(store.value.borrow().as_deref(), None);
    }

    #[test]
    fn login_propagates_store_errors() {
        let store = FakeStore {
            value: RefCell::new(None),
            fail: true,
        };
        assert!(login(&store, "ops_abc").is_err());
    }
}

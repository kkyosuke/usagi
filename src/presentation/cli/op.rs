//! `usagi op login`: store the 1Password service-account token usagi uses to
//! resolve workspace `op://` references.
//!
//! The token is stored in the OS secret store (never in `settings.json`). When a
//! pane launches, the env resolver reads it back and passes it to `op read`, so
//! 1Password references resolve even without an interactive `op signin` session.
//!
//! The genuine IO is injected so the orchestration is unit-tested: the OS secret
//! store ([`SecretStore`]) and the no-echo token reader are parameters, supplied
//! for real at the composition root (`main.rs`) and faked in tests.

use std::io::Write;

use anyhow::{Context, Result};
use clap::Subcommand;

use crate::infrastructure::secret_store::SecretStore;
use crate::usecase::op_auth;

const LOGIN_PROMPT: &str = "Paste your 1Password service account token, then press Enter:";
const LOGIN_DONE: &str = "Stored the token in the OS keychain.";

/// `usagi op <subcommand>`.
#[derive(Subcommand)]
pub enum OpCommand {
    /// Store a 1Password service account token in the OS keychain
    Login,
}

/// Entry point for `usagi op`.
///
/// `store` is the OS secret store, `read_token` a no-echo reader for the token,
/// and `output` where human-facing messages are written. All are injected so
/// this flow is exercised in tests without a real keychain or terminal.
pub fn run(
    command: OpCommand,
    store: &dyn SecretStore,
    read_token: Option<Box<dyn FnOnce() -> Result<String>>>,
    output: &mut dyn Write,
) -> Result<()> {
    match command {
        OpCommand::Login => {
            writeln!(output, "{LOGIN_PROMPT}")?;
            let read_token =
                read_token.ok_or_else(|| anyhow::anyhow!("token reader is required"))?;
            let token = read_token().context("reading the token")?;
            op_auth::login(store, &token)
                .map_err(|e| anyhow::anyhow!("failed to store the token: {e}"))?;
            writeln!(output, "{LOGIN_DONE}")?;
        }
    }
    Ok(())
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

    impl FakeStore {
        fn failing() -> Self {
            Self {
                value: RefCell::new(None),
                fail: true,
            }
        }
    }

    impl SecretStore for FakeStore {
        fn set(&self, _key: &str, value: &str) -> Result<(), String> {
            if self.fail {
                return Err("boom".to_string());
            }
            self.value.replace(Some(value.to_string()));
            Ok(())
        }
    }

    fn output_string(buf: Vec<u8>) -> String {
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn login_stores_the_token() {
        let store = FakeStore::default();
        let mut out = Vec::new();
        run(
            OpCommand::Login,
            &store,
            Some(Box::new(|| Ok("ops_abc".to_string()))),
            &mut out,
        )
        .unwrap();
        assert_eq!(store.value.borrow().as_deref(), Some("ops_abc"));
        let printed = output_string(out);
        assert!(printed.contains("Paste your 1Password"));
        assert!(printed.contains("Stored the token"));
    }

    #[test]
    fn login_with_a_blank_token_fails() {
        let store = FakeStore::default();
        let mut out = Vec::new();
        let err = run(
            OpCommand::Login,
            &store,
            Some(Box::new(|| Ok("   ".to_string()))),
            &mut out,
        )
        .unwrap_err();
        assert!(err.to_string().contains("failed to store the token"));
        assert_eq!(store.value.borrow().as_deref(), None);
    }

    #[test]
    fn login_propagates_a_token_reader_error() {
        let store = FakeStore::default();
        let mut out = Vec::new();
        let err = run(
            OpCommand::Login,
            &store,
            Some(Box::new(|| Err(anyhow::anyhow!("no tty")))),
            &mut out,
        )
        .unwrap_err();
        assert!(err.to_string().contains("reading the token"));
    }

    #[test]
    fn login_requires_a_token_reader() {
        let store = FakeStore::default();
        let mut out = Vec::new();
        let err = run(OpCommand::Login, &store, None, &mut out).unwrap_err();
        assert!(err.to_string().contains("token reader is required"));
    }

    #[test]
    fn login_surfaces_store_failures() {
        let store = FakeStore::failing();
        let mut out = Vec::new();
        let result = run(
            OpCommand::Login,
            &store,
            Some(Box::new(|| Ok("ops_abc".to_string()))),
            &mut out,
        );
        assert!(result.is_err());
    }

    #[test]
    fn clap_metadata_lists_the_op_subcommands() {
        // Exercise the `#[derive(Subcommand)]` code clap generates for this enum
        // so the CLI metadata stays covered alongside the runtime flow.
        let command = <OpCommand as clap::Subcommand>::augment_subcommands(
            clap::Command::new("op").disable_help_subcommand(true),
        );
        let names: Vec<&str> = command
            .get_subcommands()
            .map(clap::Command::get_name)
            .collect();
        assert_eq!(names, vec!["login"]);
        let update = <OpCommand as clap::Subcommand>::augment_subcommands_for_update(
            clap::Command::new("op"),
        );
        assert!(update.has_subcommands());
    }
}

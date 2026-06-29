//! `usagi op`: manage the 1Password credential `usagi op-mcp` uses.
//!
//! The 1Password **service account token** is stored in the OS secret store
//! (macOS Keychain, Windows Credential Manager, Linux Secret Service / kernel
//! keyring), never in `settings.json`. These subcommands store it (`login`),
//! remove it (`logout`), and report whether it is present (`status`) — and keep
//! the non-secret `op_mcp.enabled` flag in settings in sync so launched agents
//! know whether to wire the `usagi-op` MCP server.
//!
//! The genuine IO is injected so the orchestration is unit-tested: the OS secret
//! store ([`SecretStore`]) and the no-echo token reader are parameters, supplied
//! for real at the composition root (`main.rs`) and faked in tests.

use std::io::Write;

use anyhow::{Context, Result};
use clap::Subcommand;

use crate::infrastructure::secret_store::SecretStore;
use crate::infrastructure::storage::Storage;
use crate::usecase::op_auth;

const LOGIN_PROMPT: &str = "Paste your 1Password service account token, then press Enter:";
const LOGIN_DONE: &str = "Stored the token in the OS keychain and enabled op-mcp.";
const LOGOUT_DONE: &str = "Removed the token from the OS keychain and disabled op-mcp.";

/// `usagi op <subcommand>`.
#[derive(Subcommand)]
pub enum OpCommand {
    /// Store a 1Password service account token in the OS keychain and enable op-mcp
    Login,
    /// Remove the stored token from the OS keychain and disable op-mcp
    Logout,
    /// Show whether op-mcp is enabled and a token is stored
    Status,
}

/// Entry point for `usagi op`.
///
/// `store` is the OS secret store, `storage` the settings store, `read_token` a
/// no-echo reader used only by `login`, and `output` where human-facing messages
/// are written. All are injected so this flow is exercised in tests without a
/// real keychain or terminal.
pub fn run(
    command: OpCommand,
    store: &dyn SecretStore,
    storage: &Storage,
    read_token: Option<Box<dyn FnOnce() -> Result<String>>>,
    output: &mut dyn Write,
) -> Result<()> {
    match command {
        OpCommand::Login => {
            let settings = storage.load_settings()?;
            writeln!(output, "{LOGIN_PROMPT}")?;
            let read_token =
                read_token.ok_or_else(|| anyhow::anyhow!("token reader is required"))?;
            let token = read_token().context("reading the token")?;
            let updated = op_auth::login(store, settings, &token)
                .map_err(|e| anyhow::anyhow!("failed to store the token: {e}"))?;
            storage.save_settings(&updated)?;
            writeln!(output, "{LOGIN_DONE}")?;
        }
        OpCommand::Logout => {
            let settings = storage.load_settings()?;
            let updated = op_auth::logout(store, settings)
                .map_err(|e| anyhow::anyhow!("failed to remove the token: {e}"))?;
            storage.save_settings(&updated)?;
            writeln!(output, "{LOGOUT_DONE}")?;
        }
        OpCommand::Status => {
            let settings = storage.load_settings()?;
            let status = op_auth::status(store, &settings)
                .map_err(|e| anyhow::anyhow!("failed to read the keychain: {e}"))?;
            writeln!(output, "op-mcp enabled:    {}", status.enabled)?;
            writeln!(output, "token in keychain: {}", status.token_present)?;
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
        fn get(&self, _key: &str) -> Result<Option<String>, String> {
            if self.fail {
                return Err("boom".to_string());
            }
            Ok(self.value.borrow().clone())
        }
        fn set(&self, _key: &str, value: &str) -> Result<(), String> {
            if self.fail {
                return Err("boom".to_string());
            }
            self.value.replace(Some(value.to_string()));
            Ok(())
        }
        fn delete(&self, _key: &str) -> Result<(), String> {
            if self.fail {
                return Err("boom".to_string());
            }
            self.value.replace(None);
            Ok(())
        }
    }

    fn storage() -> (tempfile::TempDir, Storage) {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::new(dir.path().join("usagi"));
        (dir, storage)
    }

    fn output_string(buf: Vec<u8>) -> String {
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn login_stores_the_token_and_enables_the_flag() {
        let store = FakeStore::default();
        let (_dir, storage) = storage();
        let mut out = Vec::new();
        run(
            OpCommand::Login,
            &store,
            &storage,
            Some(Box::new(|| Ok("ops_abc".to_string()))),
            &mut out,
        )
        .unwrap();
        assert!(storage.load_settings().unwrap().op_mcp.enabled);
        assert_eq!(store.value.borrow().as_deref(), Some("ops_abc"));
        let printed = output_string(out);
        assert!(printed.contains("Paste your 1Password"));
        assert!(printed.contains("enabled op-mcp"));
    }

    #[test]
    fn login_with_a_blank_token_fails_and_does_not_enable() {
        let store = FakeStore::default();
        let (_dir, storage) = storage();
        let mut out = Vec::new();
        let err = run(
            OpCommand::Login,
            &store,
            &storage,
            Some(Box::new(|| Ok("   ".to_string()))),
            &mut out,
        )
        .unwrap_err();
        assert!(err.to_string().contains("failed to store the token"));
        assert!(!storage.load_settings().unwrap().op_mcp.enabled);
    }

    #[test]
    fn login_propagates_a_token_reader_error() {
        let store = FakeStore::default();
        let (_dir, storage) = storage();
        let mut out = Vec::new();
        let err = run(
            OpCommand::Login,
            &store,
            &storage,
            Some(Box::new(|| Err(anyhow::anyhow!("no tty")))),
            &mut out,
        )
        .unwrap_err();
        assert!(err.to_string().contains("reading the token"));
    }

    #[test]
    fn login_requires_a_token_reader() {
        let store = FakeStore::default();
        let (_dir, storage) = storage();
        let mut out = Vec::new();
        let err = run(OpCommand::Login, &store, &storage, None, &mut out).unwrap_err();
        assert!(err.to_string().contains("token reader is required"));
    }

    #[test]
    fn logout_clears_the_token_and_disables_the_flag() {
        let store = FakeStore {
            value: RefCell::new(Some("ops_abc".to_string())),
            fail: false,
        };
        let (_dir, storage) = storage();
        // Start enabled so logout has something to turn off.
        let mut settings = storage.load_settings().unwrap();
        settings.op_mcp.enabled = true;
        storage.save_settings(&settings).unwrap();
        let mut out = Vec::new();
        run(OpCommand::Logout, &store, &storage, None, &mut out).unwrap();
        assert!(!storage.load_settings().unwrap().op_mcp.enabled);
        assert_eq!(store.value.borrow().as_deref(), None);
        assert!(output_string(out).contains("disabled op-mcp"));
    }

    #[test]
    fn status_reports_the_flag_and_token_presence() {
        let store = FakeStore {
            value: RefCell::new(Some("ops_abc".to_string())),
            fail: false,
        };
        let (_dir, storage) = storage();
        let mut settings = storage.load_settings().unwrap();
        settings.op_mcp.enabled = true;
        storage.save_settings(&settings).unwrap();
        let mut out = Vec::new();
        run(OpCommand::Status, &store, &storage, None, &mut out).unwrap();
        let printed = output_string(out);
        assert!(printed.contains("op-mcp enabled:    true"));
        assert!(printed.contains("token in keychain: true"));
    }

    #[test]
    fn store_failures_surface_for_each_subcommand() {
        let (_dir, storage) = storage();
        for command in [OpCommand::Login, OpCommand::Logout, OpCommand::Status] {
            let store = FakeStore::failing();
            let mut out = Vec::new();
            let read_token =
                match command {
                    OpCommand::Login => Some(Box::new(|| Ok("ops_abc".to_string()))
                        as Box<dyn FnOnce() -> Result<String>>),
                    OpCommand::Logout | OpCommand::Status => None,
                };
            let result = run(command, &store, &storage, read_token, &mut out);
            assert!(result.is_err());
        }
    }

    #[test]
    fn clap_metadata_lists_the_op_subcommands() {
        // Exercise the `#[derive(Subcommand)]` code that clap generates for this
        // enum so the CLI metadata stays covered alongside the runtime flow.
        let command = <OpCommand as clap::Subcommand>::augment_subcommands(
            clap::Command::new("op").disable_help_subcommand(true),
        );
        let names: Vec<&str> = command
            .get_subcommands()
            .map(clap::Command::get_name)
            .collect();
        assert_eq!(names, vec!["login", "logout", "status"]);
        let update = <OpCommand as clap::Subcommand>::augment_subcommands_for_update(
            clap::Command::new("op"),
        );
        assert!(update.has_subcommands());
    }
}

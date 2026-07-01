//! Resolve workspace-scoped secret environment variables before launching a pane.
//!
//! Workspaces store only `NAME = op://vault/item/field` references in
//! [`LocalSettings`](crate::domain::settings::LocalSettings). This module turns
//! those references into actual secret values just-in-time for an embedded agent
//! or terminal process and returns a plain environment map the PTY layer can put
//! on the child process. Failed reads are reported to the error log and omitted;
//! a missing or locked 1Password account should not make the pane impossible to
//! open.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::domain::settings::LocalSettings;
use crate::infrastructure::secret_store::OP_SERVICE_ACCOUNT_TOKEN_KEY;
use crate::presentation::mcp::child_io::{read_capped, wait_with_timeout, WaitableChild};

const OP_TIMEOUT: Duration = Duration::from_secs(30);
const OP_POLL: Duration = Duration::from_millis(50);
const MAX_OP_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_OP_STDERR_BYTES: usize = 4 * 1024;

/// Resolve the secret environment configured for `workspace_root`.
///
/// The returned map contains only variables whose name/reference pass
/// [`LocalSettings::env`] and whose `op read --no-newline` call succeeds. Errors
/// are logged with the variable name and reference but never the resolved secret.
pub fn resolve_workspace_env(workspace_root: &Path) -> BTreeMap<String, String> {
    let settings = crate::usecase::settings::load_local(workspace_root).unwrap_or_default();
    resolve_env(&settings, &OpCliResolver)
}

/// Resolve `settings.env` through `resolver`. Public so the behaviour is covered
/// without shelling out to the real `op` CLI.
pub fn resolve_env(
    settings: &LocalSettings,
    resolver: &dyn SecretResolver,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for (name, reference) in settings.env() {
        match resolver.read(reference) {
            Ok(value) => {
                env.insert(name.to_string(), value);
            }
            Err(error) => crate::infrastructure::error_log::ErrorLog::record(&format!(
                "failed to resolve workspace env {name} from {reference}: {error}"
            )),
        }
    }
    env
}

/// Reads one secret reference. Abstracted for unit tests.
pub trait SecretResolver {
    fn read(&self, reference: &str) -> Result<String, String>;
}

struct OpCliResolver;

impl SecretResolver for OpCliResolver {
    fn read(&self, reference: &str) -> Result<String, String> {
        op_read(reference)
    }
}

fn op_read(reference: &str) -> Result<String, String> {
    let mut command = Command::new("op");
    command
        .arg("read")
        .arg("--no-newline")
        .arg(reference)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(token) = op_service_account_token() {
        command.env("OP_SERVICE_ACCOUNT_TOKEN", token);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("failed to start op: {e}"))?;

    let mut out = child
        .stdout
        .take()
        .ok_or_else(|| "failed to open op stdout".to_string())?;
    let mut err = child
        .stderr
        .take()
        .ok_or_else(|| "failed to open op stderr".to_string())?;
    let out_reader = std::thread::spawn(move || read_capped(&mut out, MAX_OP_OUTPUT_BYTES));
    let err_reader = std::thread::spawn(move || read_capped(&mut err, MAX_OP_STDERR_BYTES));

    let status = wait_with_timeout(&mut RealChild(child), OP_TIMEOUT, OP_POLL);
    let stdout_result = out_reader
        .join()
        .unwrap_or_else(|_| Ok((Vec::new(), false)));
    let stderr_result = err_reader
        .join()
        .unwrap_or_else(|_| Ok((Vec::new(), false)));

    let Some(status) = status else {
        return Err(format!(
            "op did not finish within {OP_TIMEOUT:?} and was terminated"
        ));
    };
    let (stdout, stdout_truncated) =
        stdout_result.map_err(|e| format!("failed to read op output: {e}"))?;
    let (stderr, stderr_truncated) = stderr_result.unwrap_or((Vec::new(), false));
    if !status.success() {
        let mut detail = String::from_utf8_lossy(&stderr).trim().to_string();
        if stderr_truncated {
            detail.push_str(" …(truncated)");
        }
        if detail.is_empty() {
            detail = "no stderr".to_string();
        }
        return Err(format!("op exited with {status}: {detail}"));
    }

    let mut text = String::from_utf8_lossy(&stdout).to_string();
    if stdout_truncated {
        text.push_str(" …(truncated)");
    }
    Ok(text.trim_end_matches(['\n', '\r']).to_string())
}

/// The same OS-keychain entry `usagi op login` writes for op-mcp. Supplying it
/// to `op read` preserves non-interactive service-account auth for env injection
/// instead of requiring a separate ambient `op signin` session.
fn op_service_account_token() -> Option<String> {
    const KEYRING_SERVICE: &str = "usagi";
    let entry = keyring::Entry::new(KEYRING_SERVICE, OP_SERVICE_ACCOUNT_TOKEN_KEY).ok()?;
    match entry.get_password() {
        Ok(password) if !password.trim().is_empty() => Some(password),
        _ => None,
    }
}

struct RealChild(std::process::Child);

impl WaitableChild for RealChild {
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.0.try_wait()
    }

    fn kill(&mut self) -> std::io::Result<()> {
        self.0.kill()
    }

    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.0.wait()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct FakeResolver {
        calls: RefCell<Vec<String>>,
        fail: &'static str,
    }

    impl FakeResolver {
        fn new(fail: &'static str) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                fail,
            }
        }
    }

    impl SecretResolver for FakeResolver {
        fn read(&self, reference: &str) -> Result<String, String> {
            self.calls.borrow_mut().push(reference.to_string());
            if reference == self.fail {
                Err("nope".to_string())
            } else {
                Ok(format!("value:{reference}"))
            }
        }
    }

    #[test]
    fn resolve_env_reads_valid_bindings_and_skips_invalid_or_failed_ones() {
        let mut settings = LocalSettings::default();
        settings.env.insert(
            "GH_TOKEN".to_string(),
            "op://Private/GitHub/token".to_string(),
        );
        settings
            .env
            .insert("1BAD".to_string(), "op://Private/Bad/token".to_string());
        settings.env.insert("EMPTY".to_string(), "  ".to_string());
        settings
            .env
            .insert("FAIL".to_string(), "op://Private/Fail/token".to_string());
        let resolver = FakeResolver::new("op://Private/Fail/token");

        let env = resolve_env(&settings, &resolver);

        assert_eq!(
            resolver.calls.borrow().as_slice(),
            ["op://Private/Fail/token", "op://Private/GitHub/token"]
        );
        assert_eq!(env.len(), 1);
        assert_eq!(
            env.get("GH_TOKEN").map(String::as_str),
            Some("value:op://Private/GitHub/token")
        );
        assert!(!env.contains_key("FAIL"));
    }
}

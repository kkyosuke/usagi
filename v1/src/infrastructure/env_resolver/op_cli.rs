//! Real 1Password `op` CLI subprocess that backs
//! [`resolve_workspace_env`](super::resolve_workspace_env).
//!
//! Everything here is genuine external IO — spawning the `op` binary, streaming
//! its stdout/stderr on worker threads, and waiting with a timeout. The testable
//! resolution logic is injected via [`SecretResolver`] and lives (with its unit
//! tests) in the parent module; this file is the thin real-IO layer left after
//! that extraction, so it is excluded from coverage (see `scripts/coverage.sh`).

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use super::collect_resolved;
use crate::infrastructure::process::{self, Limits, Outcome};

const OP_TIMEOUT: Duration = Duration::from_secs(30);
const OP_POLL: Duration = Duration::from_millis(50);
const OP_TERMINATE_GRACE: Duration = Duration::from_millis(250);
const OP_REAP_GRACE: Duration = Duration::from_millis(250);
const MAX_OP_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_OP_STDERR_BYTES: usize = 4 * 1024;

/// Resolve the secret environment configured for `workspace_root`.
///
/// The returned map contains only variables whose name/reference pass
/// [`Settings::env`](crate::domain::settings::Settings::env) and whose
/// `op read --no-newline` call succeeds. Errors are logged with the variable name
/// and reference but never the resolved secret.
///
/// Each binding is resolved on its own thread: `op read` calls are independent
/// subprocesses, so fanning them out turns the total wait from the *sum* of the
/// per-binding latencies (each up to [`OP_TIMEOUT`]) into roughly the *slowest
/// single* one. A workspace with several 1Password references — the common case
/// that made launching a pane feel frozen — now resolves in one round-trip's
/// time. Completion order does not matter: the results are keyed into a
/// `BTreeMap` by name via [`collect_resolved`].
pub fn resolve_workspace_env(workspace_root: &Path) -> BTreeMap<String, String> {
    let settings = crate::usecase::settings::effective_for(workspace_root).unwrap_or_default();
    // The service account token stored by `usagi op login` (if any) is shared by
    // every binding's `op read`.
    let token = service_account_token();
    let bindings: Vec<(String, String)> = settings
        .env()
        .map(|(name, reference)| (name.to_string(), reference.to_string()))
        .collect();

    let results = std::thread::scope(|scope| {
        let token = token.as_deref();
        let handles: Vec<(String, String, _)> = bindings
            .into_iter()
            .map(|(name, reference)| {
                let for_thread = reference.clone();
                let handle = scope.spawn(move || op_read(&for_thread, token));
                (name, reference, handle)
            })
            .collect();
        handles
            .into_iter()
            .map(|(name, reference, handle)| {
                // A panicked reader thread is reported as a failed resolution
                // (logged, dropped) rather than propagated, so one bad binding
                // never takes down the pane launch.
                let outcome = handle
                    .join()
                    .unwrap_or_else(|_| Err("op read thread panicked".to_string()));
                (name, reference, outcome)
            })
            .collect::<Vec<_>>()
    });

    collect_resolved(results)
}

/// The 1Password service account token stored by `usagi op login`, if any.
///
/// Best-effort: when it is absent (or the keychain cannot be read) `op read`
/// falls back to whatever ambient authentication `op` already has (an `op signin`
/// session or an externally provided `OP_SERVICE_ACCOUNT_TOKEN`).
fn service_account_token() -> Option<String> {
    use crate::infrastructure::secret_store::{SystemSecretStore, OP_SERVICE_ACCOUNT_TOKEN_KEY};
    SystemSecretStore.get(OP_SERVICE_ACCOUNT_TOKEN_KEY)
}

fn op_read(reference: &str, service_account_token: Option<&str>) -> Result<String, String> {
    let mut command = Command::new("op");
    command
        .arg("read")
        .arg("--no-newline")
        .arg(reference)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Supplied through the environment, not a CLI argument, so the token never
    // appears in process listings.
    if let Some(token) = service_account_token {
        command.env("OP_SERVICE_ACCOUNT_TOKEN", token);
    }
    let outcome = process::run(
        command,
        None,
        Limits {
            timeout: OP_TIMEOUT,
            terminate_grace: OP_TERMINATE_GRACE,
            reap_grace: OP_REAP_GRACE,
            poll_interval: OP_POLL,
            stdout_cap: MAX_OP_OUTPUT_BYTES,
            stderr_cap: MAX_OP_STDERR_BYTES,
        },
    )
    .map_err(|e| format!("failed to start op: {e}"))?;
    let Outcome::Exited(output) = outcome else {
        return Err(format!(
            "op exceeded its {OP_TIMEOUT:?} end-to-end deadline"
        ));
    };
    let stdout = output
        .stdout
        .map_err(|e| format!("failed to read op output: {e}"))?;
    let stderr = output.stderr.unwrap_or_default();
    if !output.status.success() {
        let mut detail = String::from_utf8_lossy(&stderr.bytes).trim().to_string();
        if stderr.truncated {
            detail.push_str(" …(truncated)");
        }
        if detail.is_empty() {
            detail = "no stderr".to_string();
        }
        return Err(format!("op exited with {}: {detail}", output.status));
    }

    let mut text = String::from_utf8_lossy(&stdout.bytes).to_string();
    if stdout.truncated {
        text.push_str(" …(truncated)");
    }
    Ok(text.trim_end_matches(['\n', '\r']).to_string())
}

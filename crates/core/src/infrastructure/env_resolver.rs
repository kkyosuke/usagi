//! Parallel secret resolution, and the real 1Password CLI (`op`) behind it.
//!
//! The keep/drop policy and the literal-vs-reference decision live in
//! [`usecase::env`](crate::usecase::env). This module adds the two things that
//! belong to the outside world: reading many references **at once**, and the
//! `op read` subprocess itself.
//!
//! [`resolve_parallel`] reads every reference on its own thread because `op read`
//! calls are independent subprocesses: fanning them out turns the wait from the
//! *sum* of the per-binding latencies into roughly the *slowest single* one — the
//! difference between a pane that opens and one that feels frozen when a
//! workspace configures several secrets. It takes the resolver as a parameter, so
//! that concurrency policy is covered with a fake and only [`OpCli`] — the actual
//! subprocess — is real IO.

use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use crate::domain::settings::{EnvBindings, is_secret_reference, valid_bindings};
use crate::usecase::env::{ResolvedEnvironment, SecretResolver, collect};

/// How long one `op read` may take before its binding is reported as failed.
const OP_TIMEOUT: Duration = Duration::from_secs(30);

/// The 1Password CLI as a [`SecretResolver`].
///
/// Authentication follows the CLI's own rules: an `op signin` session, or a
/// service-account token in the daemon's own environment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpCli;

impl SecretResolver for OpCli {
    #[coverage(off)] // coverage: reason=real_io owner=core expires=2027-01-31 tests=resolve_parallel_resolves_every_binding_and_keeps_the_policy
    fn read(&self, reference: &str) -> Result<String, String> {
        op_read(reference)
    }
}

/// Resolve `bindings` through `resolver`, reading the secret references in
/// parallel.
///
/// Literal values never reach the resolver. A panicking reader thread is
/// reported as a failed binding rather than propagated, so one bad reference
/// never takes down a launch.
#[must_use]
pub fn resolve_parallel<R>(bindings: &EnvBindings, resolver: &R) -> ResolvedEnvironment
where
    R: SecretResolver + Sync,
{
    let requested: Vec<(String, String)> = valid_bindings(bindings)
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    let outcomes = std::thread::scope(|scope| {
        let handles: Vec<(String, String, _)> = requested
            .into_iter()
            .map(|(name, value)| {
                let handle = is_secret_reference(&value).then(|| {
                    let reference = value.clone();
                    scope.spawn(move || resolver.read(&reference))
                });
                (name, value, handle)
            })
            .collect();
        handles
            .into_iter()
            .map(|(name, value, handle)| {
                let outcome = match handle {
                    Some(handle) => handle
                        .join()
                        .unwrap_or_else(|_| Err("secret read thread panicked".to_owned())),
                    None => Ok(value.clone()),
                };
                (name, value, outcome)
            })
            .collect::<Vec<_>>()
    });
    collect(outcomes)
}

/// Run `op read --no-newline <reference>` and return its trimmed stdout.
///
/// The child is waited for on a worker thread so a hung `op` (an unresponsive
/// desktop app, a vault waiting on a prompt) fails its own binding after
/// [`OP_TIMEOUT`] instead of stalling the launch forever.
#[coverage(off)] // coverage: reason=real_io owner=core expires=2027-01-31 tests=resolve_parallel_resolves_every_binding_and_keeps_the_policy
fn op_read(reference: &str) -> Result<String, String> {
    let child = Command::new("op")
        .arg("read")
        .arg("--no-newline")
        .arg(reference)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to start op: {error}"))?;
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(child.wait_with_output());
    });
    let output = match receiver.recv_timeout(OP_TIMEOUT) {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => return Err(format!("failed to read op output: {error}")),
        Err(_) => return Err(format!("op exceeded its {OP_TIMEOUT:?} deadline")),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let detail = if stderr.is_empty() {
            "no stderr"
        } else {
            &stderr
        };
        return Err(format!("op exited with {}: {detail}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end_matches(['\n', '\r'])
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::{OpCli, resolve_parallel};
    use crate::domain::settings::EnvBindings;
    use crate::usecase::env::SecretResolver;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    struct FakeResolver {
        reads: Mutex<Vec<String>>,
    }

    impl SecretResolver for FakeResolver {
        fn read(&self, reference: &str) -> Result<String, String> {
            self.reads.lock().unwrap().push(reference.to_owned());
            if reference.contains("Locked") {
                Err("op is locked".to_owned())
            } else {
                Ok(format!("secret:{reference}"))
            }
        }
    }

    struct PanickingResolver;

    impl SecretResolver for PanickingResolver {
        fn read(&self, _reference: &str) -> Result<String, String> {
            panic!("resolver thread died");
        }
    }

    fn bindings(pairs: &[(&str, &str)]) -> EnvBindings {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn resolve_parallel_resolves_every_binding_and_keeps_the_policy() {
        let reader = FakeResolver {
            reads: Mutex::new(Vec::new()),
        };
        let resolved = resolve_parallel(
            &bindings(&[
                ("RUST_LOG", "debug"),
                ("OPEN", "op://Private/Open/token"),
                ("LOCKED", "op://Private/Locked/token"),
                ("1BAD", "op://Private/Bad/token"),
            ]),
            &reader,
        );

        let mut reads = reader.reads.lock().unwrap().clone();
        reads.sort();
        assert_eq!(
            reads,
            ["op://Private/Locked/token", "op://Private/Open/token"]
        );
        assert_eq!(
            resolved.values,
            BTreeMap::from([
                (
                    "OPEN".to_owned(),
                    "secret:op://Private/Open/token".to_owned()
                ),
                ("RUST_LOG".to_owned(), "debug".to_owned()),
            ])
        );
        assert_eq!(resolved.failures.len(), 1);
        assert_eq!(resolved.failures[0].name, "LOCKED");
    }

    #[test]
    fn a_panicking_reader_fails_only_its_own_binding() {
        let resolved = resolve_parallel(
            &bindings(&[("SECRET", "op://Private/Item/field"), ("LITERAL", "kept")]),
            &PanickingResolver,
        );
        assert_eq!(
            resolved.values,
            BTreeMap::from([("LITERAL".to_owned(), "kept".to_owned())])
        );
        assert_eq!(
            resolved.failures[0].error,
            "secret read thread panicked".to_owned()
        );
    }

    #[test]
    fn literal_only_bindings_never_touch_the_op_cli() {
        // `OpCli` would spawn a subprocess for a reference; a configuration
        // without references resolves without one.
        let resolved = resolve_parallel(&bindings(&[("RUST_LOG", "debug")]), &OpCli);
        assert_eq!(
            resolved.values,
            BTreeMap::from([("RUST_LOG".to_owned(), "debug".to_owned())])
        );
        assert!(resolved.failures.is_empty());
        assert!(format!("{OpCli:?}").contains("OpCli"));
        assert_eq!(OpCli, OpCli {});
    }
}

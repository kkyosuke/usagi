//! Turn configured environment bindings into a child process environment.
//!
//! usagi keeps environment bindings in two scopes — global
//! ([`Settings::env`](crate::domain::settings::Settings::env)) and per workspace
//! ([`LocalSettings::env`](crate::domain::settings::LocalSettings::env)) — and
//! merges them with the workspace on top
//! ([`Settings::with_local`](crate::domain::settings::Settings::with_local)).
//! This module owns the resolver port and the result collection policy. The
//! infrastructure adapter resolves the merged map before a PTY child is spawned:
//! literal values pass through and `op://…` references reach [`SecretResolver`].
//! The policy can therefore be tested without the real 1Password CLI.
//!
//! Once the adapter validates the configured limits, individual read failures
//! leave their binding out of the environment and are reported in
//! [`ResolvedEnvironment::failures`], so a locked 1Password account degrades one
//! variable instead of making a pane impossible to open. Failures carry the
//! variable name and its reference — never a resolved secret.

use std::collections::BTreeMap;

/// Which stored scope an environment editor reads and writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EnvScope {
    /// The per-user bindings shared by every workspace.
    Global,
    /// The current workspace's own bindings, layered over the global ones.
    Workspace,
}

impl EnvScope {
    /// A stable lowercase token for command arguments and diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Workspace => "workspace",
        }
    }
}

/// Reads one secret reference. Abstracted so the resolution policy is covered
/// without spawning the 1Password CLI.
pub trait SecretResolver {
    /// Resolve `reference` to its secret value.
    ///
    /// # Errors
    ///
    /// Returns a description of why the secret could not be read. The text must
    /// not contain the secret.
    fn read(&self, reference: &str) -> Result<String, String>;

    /// Resolve with an optional daemon-owned 1Password service-account token.
    /// Resolvers other than the 1Password adapter ignore this credential.
    ///
    /// # Errors
    ///
    /// Returns a description of why the secret could not be read. The text must
    /// not contain either the resolved secret or the service-account token.
    fn read_with_service_account_token(
        &self,
        reference: &str,
        _service_account_token: Option<&str>,
    ) -> Result<String, String> {
        self.read(reference)
    }

    /// Resolve a named binding, allowing a cache to preserve its configured scope.
    ///
    /// # Errors
    /// Returns the same secret-free failure as the credential-aware read.
    fn read_binding(
        &self,
        _name: &str,
        reference: &str,
        service_account_token: Option<&str>,
    ) -> Result<String, String> {
        self.read_with_service_account_token(reference, service_account_token)
    }
}

/// One binding that could not be resolved, safe to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvFailure {
    /// The environment variable that stays unset.
    pub name: String,
    /// The secret reference that could not be read.
    pub reference: String,
    /// Why the read failed, as reported by the resolver.
    pub error: String,
}

/// The environment values to inject, plus the bindings that were dropped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResolvedEnvironment {
    /// Variable name to resolved value, ready for the child process.
    pub values: BTreeMap<String, String>,
    /// Bindings left unset, in binding order.
    pub failures: Vec<EnvFailure>,
}

/// Fold already-resolved outcomes into a [`ResolvedEnvironment`].
///
/// The resolver supplies ordered outcomes after its bounded workers finish.
/// This is the shared keep/drop policy for those results.
/// Each item is `(name, value, outcome)`.
#[must_use]
pub fn collect<I>(outcomes: I) -> ResolvedEnvironment
where
    I: IntoIterator<Item = (String, String, Result<String, String>)>,
{
    let mut resolved = ResolvedEnvironment::default();
    for (name, reference, outcome) in outcomes {
        match outcome {
            Ok(value) => {
                resolved.values.insert(name, value);
            }
            Err(error) => resolved.failures.push(EnvFailure {
                name,
                reference,
                error,
            }),
        }
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::{EnvScope, collect};
    use std::collections::BTreeMap;

    #[test]
    fn scope_tokens_are_stable() {
        assert_eq!(EnvScope::Global.as_str(), "global");
        assert_eq!(EnvScope::Workspace.as_str(), "workspace");
        assert_eq!(EnvScope::Global, EnvScope::Global);
        assert_ne!(EnvScope::Global, EnvScope::Workspace);
        assert!(format!("{:?}", EnvScope::Workspace).contains("Workspace"));
    }

    #[test]
    fn collect_applies_the_same_policy_to_out_of_order_parallel_outcomes() {
        let resolved = collect([
            (
                "SECOND".to_owned(),
                "op://Private/Second/token".to_owned(),
                Ok("two".to_owned()),
            ),
            (
                "FIRST".to_owned(),
                "op://Private/First/token".to_owned(),
                Err("thread panicked".to_owned()),
            ),
        ]);
        assert_eq!(
            resolved.values,
            BTreeMap::from([("SECOND".to_owned(), "two".to_owned())])
        );
        assert_eq!(resolved.failures.len(), 1);
        assert_eq!(resolved.failures[0].name, "FIRST");
        assert_eq!(resolved.clone(), resolved);
    }
}

//! Turn configured environment bindings into a child process environment.
//!
//! usagi keeps environment bindings in two scopes — global
//! ([`Settings::env`](crate::domain::settings::Settings::env)) and per workspace
//! ([`LocalSettings::env`](crate::domain::settings::LocalSettings::env)) — and
//! merges them with the workspace on top
//! ([`Settings::with_local`](crate::domain::settings::Settings::with_local)).
//! This module resolves that merged map just before a PTY child is spawned: a
//! literal value is injected as-is and a `op://…` reference is read through the
//! injected [`SecretResolver`], so the resolution policy stays unit-tested
//! without shelling out to the real 1Password CLI.
//!
//! Resolution never fails as a whole. A binding whose secret cannot be read is
//! left out of the environment and reported in
//! [`ResolvedEnvironment::failures`], so a locked 1Password account degrades one
//! variable instead of making a pane impossible to open. Failures carry the
//! variable name and its reference — never a resolved secret.

use std::collections::BTreeMap;

use crate::domain::settings::{EnvBindings, is_secret_reference, valid_bindings};

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

/// Resolve `bindings` into a child process environment through `resolver`.
///
/// Literal values pass through untouched; only `op://…` references reach the
/// resolver, so a configuration without secrets resolves with no subprocess at
/// all. Unusable bindings (invalid name, blank value) are dropped by
/// [`valid_bindings`] before resolution and are not reported as failures — they
/// never described a variable to inject.
#[must_use]
pub fn resolve(bindings: &EnvBindings, resolver: &dyn SecretResolver) -> ResolvedEnvironment {
    collect(valid_bindings(bindings).map(|(name, value)| {
        let outcome = if is_secret_reference(value) {
            resolver.read(value)
        } else {
            Ok(value.to_owned())
        };
        (name.to_owned(), value.to_owned(), outcome)
    }))
}

/// Fold already-resolved outcomes into a [`ResolvedEnvironment`].
///
/// Split out from [`resolve`] so a real resolver can read its references **in
/// parallel** (one subprocess per reference) and still funnel results through
/// one place, keeping the keep/drop policy identical to the sequential path.
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
    use super::{EnvFailure, EnvScope, ResolvedEnvironment, SecretResolver, collect, resolve};
    use crate::domain::settings::EnvBindings;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    struct FakeResolver {
        reads: RefCell<Vec<String>>,
        failing: &'static str,
    }

    impl FakeResolver {
        fn new(failing: &'static str) -> Self {
            Self {
                reads: RefCell::new(Vec::new()),
                failing,
            }
        }
    }

    impl SecretResolver for FakeResolver {
        fn read(&self, reference: &str) -> Result<String, String> {
            self.reads.borrow_mut().push(reference.to_owned());
            if reference == self.failing {
                Err("op is locked".to_owned())
            } else {
                Ok(format!("secret:{reference}"))
            }
        }
    }

    fn bindings(pairs: &[(&str, &str)]) -> EnvBindings {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn scope_tokens_are_stable() {
        assert_eq!(EnvScope::Global.as_str(), "global");
        assert_eq!(EnvScope::Workspace.as_str(), "workspace");
        assert_eq!(EnvScope::Global, EnvScope::Global);
        assert_ne!(EnvScope::Global, EnvScope::Workspace);
        assert!(format!("{:?}", EnvScope::Workspace).contains("Workspace"));
    }

    #[test]
    fn literals_pass_through_and_only_references_reach_the_resolver() {
        let reader = FakeResolver::new("");
        let resolved = resolve(
            &bindings(&[
                ("RUST_LOG", "debug"),
                ("GH_TOKEN", "op://Private/GitHub/token"),
                // A bare prefix is not a reference; it stays a literal value.
                ("LITERAL", "op://"),
            ]),
            &reader,
        );

        assert_eq!(
            reader.reads.borrow().as_slice(),
            ["op://Private/GitHub/token"]
        );
        assert_eq!(
            resolved,
            ResolvedEnvironment {
                values: BTreeMap::from([
                    (
                        "GH_TOKEN".to_owned(),
                        "secret:op://Private/GitHub/token".to_owned()
                    ),
                    ("LITERAL".to_owned(), "op://".to_owned()),
                    ("RUST_LOG".to_owned(), "debug".to_owned()),
                ]),
                failures: Vec::new(),
            }
        );
    }

    #[test]
    fn an_unreadable_secret_is_reported_and_the_rest_still_resolves() {
        let reader = FakeResolver::new("op://Private/Locked/token");
        let resolved = resolve(
            &bindings(&[
                ("LOCKED", "op://Private/Locked/token"),
                ("OPEN", "op://Private/Open/token"),
                // Unusable bindings are dropped before resolution.
                ("1BAD", "op://Private/Bad/token"),
                ("BLANK", "   "),
            ]),
            &reader,
        );

        assert_eq!(
            resolved.values,
            BTreeMap::from([(
                "OPEN".to_owned(),
                "secret:op://Private/Open/token".to_owned()
            )])
        );
        assert_eq!(
            resolved.failures,
            [EnvFailure {
                name: "LOCKED".to_owned(),
                reference: "op://Private/Locked/token".to_owned(),
                error: "op is locked".to_owned(),
            }]
        );
        assert!(!format!("{resolved:?}").contains("secret:op://Private/Locked"));
    }

    #[test]
    fn empty_bindings_resolve_to_an_empty_environment() {
        let reader = FakeResolver::new("");
        assert_eq!(
            resolve(&EnvBindings::new(), &reader),
            ResolvedEnvironment::default()
        );
        assert!(reader.reads.borrow().is_empty());
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

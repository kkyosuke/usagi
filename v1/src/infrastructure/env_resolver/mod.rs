//! Resolve effective secret environment variables before launching a pane.
//!
//! usagi stores `NAME = op://vault/item/field` references either globally in
//! [`Settings`](crate::domain::settings::Settings) or per workspace in
//! [`LocalSettings`](crate::domain::settings::LocalSettings). This module turns
//! the already-merged effective settings into actual secret values just-in-time
//! for an embedded agent or terminal process and returns a plain environment map
//! the PTY layer can put on the child process. Failed reads are reported to the
//! error log and omitted; a missing or locked 1Password account should not make
//! the pane impossible to open.
//!
//! The pure resolution logic lives here behind the [`SecretResolver`] trait so it
//! is unit-tested without shelling out; the real `op` CLI subprocess IO that
//! backs [`resolve_workspace_env`] lives in [`op_cli`].

mod op_cli;

pub use op_cli::resolve_workspace_env;

use std::collections::BTreeMap;

use crate::domain::settings::Settings;

/// Resolve `settings.env` through `resolver`. Public so the behaviour is covered
/// without shelling out to the real `op` CLI.
pub fn resolve_env(settings: &Settings, resolver: &dyn SecretResolver) -> BTreeMap<String, String> {
    collect_resolved(settings.env().map(|(name, reference)| {
        let outcome = resolver.read(reference);
        (name.to_string(), reference.to_string(), outcome)
    }))
}

/// Fold already-resolved secret outcomes into an environment map, logging (and
/// dropping) the ones that failed. Each item is `(name, reference, outcome)`: a
/// successful `outcome` is inserted under `name`, a failure is recorded to the
/// error log with the variable's name and reference — never the resolved secret.
///
/// Split out from [`resolve_env`] so the real `op` CLI layer can resolve the
/// bindings **in parallel** (one subprocess per reference) and still funnel the
/// results through this one place, keeping the insert/log policy identical to the
/// sequential path.
pub fn collect_resolved<I>(results: I) -> BTreeMap<String, String>
where
    I: IntoIterator<Item = (String, String, Result<String, String>)>,
{
    let mut env = BTreeMap::new();
    for (name, reference, outcome) in results {
        match outcome {
            Ok(value) => {
                env.insert(name, value);
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
        let mut settings = Settings::default();
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

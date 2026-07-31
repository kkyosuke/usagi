//! Environment-variable bindings shared by global and workspace settings.
//!
//! A binding is `NAME -> value`, where the value is either a literal string
//! injected as-is or a `op://vault/item/field` secret reference resolved through
//! the 1Password CLI just before a child process is spawned. Only the binding —
//! never a resolved secret — is persisted, so a settings file stays safe to read
//! and to copy between machines.
//!
//! This module owns the binding vocabulary: which names are portable, which
//! values are secret references, and how an editor buffer maps to and from the
//! stored map. [`Settings::env`](super::Settings::env) and
//! [`LocalSettings::env`](super::LocalSettings::env) hold the two scopes; the
//! merge of the two is [`Settings::with_local`](super::Settings::with_local).

use std::collections::BTreeMap;
use std::fmt;

/// Environment bindings keyed by variable name, in name order.
///
/// The value is a literal to inject verbatim, or a [secret
/// reference](is_secret_reference) resolved at launch.
pub type EnvBindings = BTreeMap<String, String>;

/// Value prefix marking a binding as a 1Password secret reference.
pub const SECRET_REFERENCE_PREFIX: &str = "op://";

/// Maximum bindings accepted in one stored scope or one effective launch.
pub const MAX_ENV_BINDINGS: usize = 128;

/// Maximum 1Password references accepted in one stored scope or one effective launch.
pub const MAX_SECRET_REFERENCES: usize = 32;

/// Maximum `op read` children owned by one environment resolution at a time.
pub const MAX_CONCURRENT_SECRET_READS: usize = 4;

/// A settings document cannot be admitted without exceeding a resource bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvLimitError {
    TooManyBindings,
    TooManySecretReferences,
}

impl fmt::Display for EnvLimitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyBindings => write!(
                formatter,
                "environment settings exceed the {MAX_ENV_BINDINGS} binding limit"
            ),
            Self::TooManySecretReferences => write!(
                formatter,
                "environment settings exceed the {MAX_SECRET_REFERENCES} secret reference limit"
            ),
        }
    }
}

impl std::error::Error for EnvLimitError {}

/// Enforce the resource limits shared by domain, storage, and launch admission.
///
/// Counts use the stored map rather than only valid bindings. A hand-edited file
/// cannot evade the resource bound by padding it with invalid names.
///
/// # Errors
/// Returns the first binding or secret-reference limit that is exceeded.
pub fn validate_env_limits(bindings: &EnvBindings) -> Result<(), EnvLimitError> {
    if bindings.len() > MAX_ENV_BINDINGS {
        return Err(EnvLimitError::TooManyBindings);
    }
    if bindings
        .values()
        .filter(|value| is_secret_reference(value.trim()))
        .count()
        > MAX_SECRET_REFERENCES
    {
        return Err(EnvLimitError::TooManySecretReferences);
    }
    Ok(())
}

/// Whether `name` is a portable environment variable name. Deliberately strict,
/// so a hand-edited settings file cannot smuggle shell syntax into a child
/// environment or into a diagnostic line.
#[must_use]
pub fn is_valid_env_name(name: &str) -> bool {
    let mut characters = name.chars();
    match characters.next() {
        Some(first) if first == '_' || first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

/// Whether `value` names a secret to read from 1Password rather than a literal
/// to inject. The bare prefix with no path is not a usable reference.
#[must_use]
pub fn is_secret_reference(value: &str) -> bool {
    value
        .strip_prefix(SECRET_REFERENCE_PREFIX)
        .is_some_and(|path| !path.trim().is_empty())
}

/// The bindings that are safe to inject: a portable name and a non-blank value,
/// both trimmed. Anything else is dropped, so what an editor saves is exactly
/// what a launch injects.
pub fn valid_bindings(bindings: &EnvBindings) -> impl Iterator<Item = (&str, &str)> {
    bindings.iter().filter_map(|(name, value)| {
        let name = name.trim();
        let value = value.trim();
        (is_valid_env_name(name) && !value.is_empty() && !value.contains('\0'))
            .then_some((name, value))
    })
}

/// Parse a `NAME=value` editor buffer (one binding per line) into bindings.
///
/// Keyed by name, so a later line with the same name wins and the result keeps
/// its sorted order. Lines without a `=`, with a name that is not a portable
/// identifier, or with a blank value are dropped — the same filter
/// [`valid_bindings`] applies when a launch reads them back.
#[must_use]
pub fn parse_env_bindings(text: &str) -> EnvBindings {
    let mut bindings = EnvBindings::new();
    for line in text.lines() {
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let candidate = EnvBindings::from([(name.to_owned(), value.to_owned())]);
        for (name, value) in valid_bindings(&candidate) {
            bindings.insert(name.to_owned(), value.to_owned());
        }
    }
    bindings
}

/// Render bindings back into the editor buffer form ([`parse_env_bindings`]'s
/// inverse): one `NAME=value` line per binding, in name order.
#[must_use]
pub fn format_env_bindings(bindings: &EnvBindings) -> String {
    bindings
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::{
        EnvBindings, EnvLimitError, MAX_ENV_BINDINGS, MAX_SECRET_REFERENCES, format_env_bindings,
        is_secret_reference, is_valid_env_name, parse_env_bindings, valid_bindings,
        validate_env_limits,
    };

    #[test]
    fn portable_names_are_accepted_and_shell_syntax_is_refused() {
        for name in ["GH_TOKEN", "_hidden", "A1"] {
            assert!(is_valid_env_name(name), "{name} should be portable");
        }
        for name in ["", "1BAD", "WITH SPACE", "WITH-DASH", "PATH;ls", "日本語"] {
            assert!(!is_valid_env_name(name), "{name} should be refused");
        }
    }

    #[test]
    fn only_a_prefixed_reference_with_a_path_is_a_secret() {
        assert!(is_secret_reference("op://Private/GitHub/token"));
        assert!(!is_secret_reference("op://"));
        assert!(!is_secret_reference("op://   "));
        assert!(!is_secret_reference("debug"));
    }

    #[test]
    fn valid_bindings_trim_and_drop_unusable_entries() {
        let bindings = EnvBindings::from([
            (
                " GH_TOKEN ".to_owned(),
                " op://Private/GitHub/token ".to_owned(),
            ),
            ("RUST_LOG".to_owned(), "debug".to_owned()),
            ("1BAD".to_owned(), "value".to_owned()),
            ("EMPTY".to_owned(), "   ".to_owned()),
            ("NUL".to_owned(), "va\0lue".to_owned()),
        ]);
        assert_eq!(
            valid_bindings(&bindings).collect::<Vec<_>>(),
            [
                ("GH_TOKEN", "op://Private/GitHub/token"),
                ("RUST_LOG", "debug"),
            ]
        );
    }

    #[test]
    fn an_editor_buffer_round_trips_through_the_stored_map() {
        let bindings = parse_env_bindings(
            "GH_TOKEN = op://Private/GitHub/token\nRUST_LOG=debug\nRUST_LOG=trace\nnot a binding\n1BAD=x\nEMPTY=\n",
        );
        assert_eq!(
            bindings,
            EnvBindings::from([
                (
                    "GH_TOKEN".to_owned(),
                    "op://Private/GitHub/token".to_owned()
                ),
                ("RUST_LOG".to_owned(), "trace".to_owned()),
            ])
        );
        assert_eq!(
            format_env_bindings(&bindings),
            "GH_TOKEN=op://Private/GitHub/token\nRUST_LOG=trace"
        );
        assert_eq!(
            parse_env_bindings(&format_env_bindings(&bindings)),
            bindings
        );
        assert!(format_env_bindings(&EnvBindings::new()).is_empty());
    }

    #[test]
    fn binding_and_secret_limits_are_one_domain_contract() {
        let at_binding_limit = (0..MAX_ENV_BINDINGS)
            .map(|index| (format!("VALUE_{index}"), "literal".to_owned()))
            .collect();
        assert_eq!(validate_env_limits(&at_binding_limit), Ok(()));

        let over_binding_limit = (0..=MAX_ENV_BINDINGS)
            .map(|index| (format!("VALUE_{index}"), "literal".to_owned()))
            .collect();
        assert_eq!(
            validate_env_limits(&over_binding_limit),
            Err(EnvLimitError::TooManyBindings)
        );

        let at_secret_limit = (0..MAX_SECRET_REFERENCES)
            .map(|index| (format!("SECRET_{index}"), format!("op://vault/{index}")))
            .collect();
        assert_eq!(validate_env_limits(&at_secret_limit), Ok(()));

        let over_secret_limit = (0..=MAX_SECRET_REFERENCES)
            .map(|index| (format!("SECRET_{index}"), format!("op://vault/{index}")))
            .collect();
        assert_eq!(
            validate_env_limits(&over_secret_limit),
            Err(EnvLimitError::TooManySecretReferences)
        );
        assert_eq!(
            EnvLimitError::TooManyBindings.to_string(),
            format!("environment settings exceed the {MAX_ENV_BINDINGS} binding limit")
        );
        assert_eq!(
            EnvLimitError::TooManySecretReferences.to_string(),
            format!(
                "environment settings exceed the {MAX_SECRET_REFERENCES} secret reference limit"
            )
        );
    }
}

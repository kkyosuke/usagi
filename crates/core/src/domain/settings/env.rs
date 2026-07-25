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

/// Environment bindings keyed by variable name, in name order.
///
/// The value is a literal to inject verbatim, or a [secret
/// reference](is_secret_reference) resolved at launch.
pub type EnvBindings = BTreeMap<String, String>;

/// Value prefix marking a binding as a 1Password secret reference.
pub const SECRET_REFERENCE_PREFIX: &str = "op://";

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
        EnvBindings, format_env_bindings, is_secret_reference, is_valid_env_name,
        parse_env_bindings, valid_bindings,
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
}

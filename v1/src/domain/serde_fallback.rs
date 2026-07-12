//! Deserialize helpers that degrade a single bad field instead of failing the
//! whole file.
//!
//! `settings.json` and `state.json` carry enum-valued fields (the agent CLI, a
//! theme, a branch's lifecycle status, …). With serde's default behaviour an
//! unrecognised enum value — one written by a *newer* usagi, or a hand-edited
//! typo — makes deserialization fail the **entire** struct, so a downgraded (or
//! merely older) usagi would refuse to load all of its settings or its whole
//! session list over one unknown word. JSON is forward-compatible by design
//! elsewhere in the codebase (frontmatter ignores unknown keys); these helpers
//! bring the JSON enums to parity by degrading the offending field to its
//! default — the same "reset a bad value to the default" stance as
//! [`crate::domain::settings::Settings::sanitized`].

use serde::{Deserialize, Deserializer};

/// Deserialize `T`, falling back to [`Default`] when the stored value is not a
/// recognised `T` (e.g. an enum variant a newer usagi introduced, or a typo).
///
/// Use as a field attribute paired with a struct- or field-level `default`, so a
/// *missing* field also resolves to the default rather than erroring:
///
/// ```ignore
/// #[serde(default, deserialize_with = "crate::domain::serde_fallback::or_default")]
/// pub theme: Theme,
/// ```
pub fn or_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    // Buffer the value first so a failed interpretation can be swallowed: a
    // deserializer cannot be "rewound" after it errors mid-stream, but a
    // fully-read `serde_json::Value` can be re-interpreted (and discarded) freely.
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(T::deserialize(value).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum Color {
        #[default]
        Red,
        Green,
    }

    #[derive(Debug, PartialEq, Deserialize)]
    #[serde(default)]
    struct Holder {
        #[serde(deserialize_with = "or_default")]
        color: Color,
        keep: u32,
    }

    impl Default for Holder {
        fn default() -> Self {
            Self {
                color: Color::Red,
                keep: 7,
            }
        }
    }

    #[test]
    fn keeps_a_known_value() {
        let h: Holder = serde_json::from_str(r#"{"color":"green","keep":1}"#).unwrap();
        assert_eq!(
            h,
            Holder {
                color: Color::Green,
                keep: 1
            }
        );
    }

    #[test]
    fn falls_back_to_default_on_an_unknown_value() {
        // The unknown color degrades to the default, and — crucially — the rest
        // of the struct still loads instead of the whole parse failing.
        let h: Holder = serde_json::from_str(r#"{"color":"chartreuse","keep":3}"#).unwrap();
        assert_eq!(
            h,
            Holder {
                color: Color::Red,
                keep: 3
            }
        );
    }

    #[test]
    fn a_missing_field_uses_the_struct_default() {
        let h: Holder = serde_json::from_str(r#"{"keep":5}"#).unwrap();
        assert_eq!(
            h,
            Holder {
                color: Color::Red,
                keep: 5
            }
        );
    }
}

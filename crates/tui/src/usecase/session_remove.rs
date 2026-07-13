//! Session removal command grammar shared by Overview and Closeup.
//!
//! The parser deliberately does not resolve a display name to an identity.
//! That resolution belongs to the snapshot-owning controller/runtime, where a
//! stale or ambiguous label can be rejected safely.

/// Parsed, presentation-neutral session removal request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveRequest {
    /// Optional display-name target. Absent means open the session selector.
    pub target: Option<String>,
    /// Whether daemon removal may discard uncommitted changes.
    pub force: bool,
}

/// Parse `[target] [-f|--force]` without accepting ambiguous destructive input.
///
/// # Errors
///
/// Returns a stable, user-safe validation message for unknown flags, duplicate
/// force flags, or more than one positional target.
pub fn parse(arguments: &str) -> Result<RemoveRequest, &'static str> {
    let mut target = None;
    let mut force = false;
    for token in arguments.split_whitespace() {
        match token {
            "-f" | "--force" if !force => force = true,
            "-f" | "--force" => return Err("force flag must not be repeated"),
            token if token.starts_with('-') => return Err("unknown remove flag"),
            token if target.is_none() => target = Some(token.to_owned()),
            _ => return Err("session remove accepts at most one target"),
        }
    }
    Ok(RemoveRequest { target, force })
}

/// Returns force-flag completion candidates not already present in `arguments`.
#[must_use]
pub fn force_completions(arguments: &str) -> Vec<&'static str> {
    (!arguments
        .split_whitespace()
        .any(|token| matches!(token, "-f" | "--force")))
    .then_some(vec!["--force", "-f"])
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{RemoveRequest, force_completions, parse};

    #[test]
    fn accepts_one_target_and_one_force_flag_in_any_order() {
        for input in ["demo -f", "-f demo", "demo --force", "--force demo"] {
            assert_eq!(
                parse(input),
                Ok(RemoveRequest {
                    target: Some("demo".into()),
                    force: true,
                })
            );
        }
        assert_eq!(
            parse(""),
            Ok(RemoveRequest {
                target: None,
                force: false,
            })
        );
    }

    #[test]
    fn rejects_ambiguous_or_unknown_destructive_input() {
        assert_eq!(parse("-x"), Err("unknown remove flag"));
        assert_eq!(
            parse("one two"),
            Err("session remove accepts at most one target")
        );
        assert_eq!(parse("-f --force"), Err("force flag must not be repeated"));
    }

    #[test]
    fn does_not_offer_a_duplicate_force_flag() {
        assert_eq!(force_completions("demo"), ["--force", "-f"]);
        assert!(force_completions("demo -f").is_empty());
    }
}

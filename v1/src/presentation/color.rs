//! Honors the `NO_COLOR` convention for usagi's terminal output.
//!
//! usagi styles its CLI and TUI output through the [`console`] crate, whose
//! built-in detection looks at `CLICOLOR` / `CLICOLOR_FORCE` and whether stdout
//! is a TTY — but **not** at [`NO_COLOR`](https://no-color.org/), the widely
//! adopted opt-out. This module decides, from the environment, whether colours
//! should be suppressed; the binary's composition root (`src/main.rs`) reads the
//! real environment and calls [`console::set_colors_enabled`] accordingly, so the
//! decision logic here stays pure and unit-testable.

/// Whether `CLICOLOR_FORCE` is set to a value that *forces* colour on.
///
/// Following the de-facto convention, any value other than the empty string or
/// `"0"` forces colour. An unset variable does not force.
fn clicolor_force_on(clicolor_force: Option<&str>) -> bool {
    matches!(clicolor_force, Some(v) if !v.is_empty() && v != "0")
}

/// Whether colour output should be disabled, given the values of the `NO_COLOR`
/// and `CLICOLOR_FORCE` environment variables (each `None` when unset).
///
/// Per [no-color.org](https://no-color.org/), colour is suppressed whenever
/// `NO_COLOR` is present with a **non-empty** value (an empty value does not
/// count). `CLICOLOR_FORCE` takes precedence: when it forces colour on,
/// `NO_COLOR` is ignored — matching how most tools resolve the two when both are
/// set.
pub fn should_disable_colors(no_color: Option<&str>, clicolor_force: Option<&str>) -> bool {
    let no_color_set = matches!(no_color, Some(v) if !v.is_empty());
    no_color_set && !clicolor_force_on(clicolor_force)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_no_color_keeps_colour() {
        assert!(!should_disable_colors(None, None));
    }

    #[test]
    fn empty_no_color_does_not_count() {
        // no-color.org: an empty value does not request no-colour.
        assert!(!should_disable_colors(Some(""), None));
    }

    #[test]
    fn non_empty_no_color_disables_colour() {
        assert!(should_disable_colors(Some("1"), None));
        // Any non-empty value works, regardless of what it is.
        assert!(should_disable_colors(Some("anything"), None));
    }

    #[test]
    fn clicolor_force_overrides_no_color() {
        // A forcing CLICOLOR_FORCE keeps colour even when NO_COLOR is set.
        assert!(!should_disable_colors(Some("1"), Some("1")));
        assert!(!should_disable_colors(Some("1"), Some("true")));
    }

    #[test]
    fn clicolor_force_zero_or_empty_does_not_force() {
        // `0` / empty are not a force, so NO_COLOR still wins.
        assert!(should_disable_colors(Some("1"), Some("0")));
        assert!(should_disable_colors(Some("1"), Some("")));
    }
}

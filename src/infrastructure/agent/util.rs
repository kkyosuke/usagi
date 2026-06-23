//! Shared helpers for the agent adapters.
//!
//! Several adapters render their launch command into a `sh -c` line and locate a
//! worktree's prior session by comparing directory paths. The shell-quoting and
//! path-comparison idioms are identical across them, so they live here once
//! rather than being copied per adapter.

use std::path::Path;

/// Wrap `text` as a single shell argument in single quotes, safe to drop into a
/// `sh -c` command line. A single quote cannot appear inside a single-quoted
/// string, so each one is rendered as `'\''` (close the quote, an escaped quote,
/// reopen) — the standard POSIX idiom. Everything else (newlines, `$`, spaces,
/// the `[`, `]`, `"` of a TOML value …) is literal inside single quotes, so the
/// agent receives the argument verbatim.
pub(super) fn shell_single_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Whether two paths name the same directory, comparing canonicalized forms (so a
/// symlinked or `/tmp` ⇄ `/private/tmp` difference still matches) and falling back
/// to a plain comparison when a path cannot be canonicalized (e.g. the recorded
/// directory no longer exists).
pub(super) fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(x), Ok(y)) if x == y
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_single_quote_wraps_and_escapes() {
        assert_eq!(shell_single_quote("plain"), "'plain'");
        // An embedded single quote closes, escapes, and reopens the quoting.
        assert_eq!(shell_single_quote("a'b"), r"'a'\''b'");
        // Shell metacharacters stay literal inside single quotes.
        assert_eq!(shell_single_quote("$x `y` \"z\""), "'$x `y` \"z\"'");
    }

    #[test]
    fn same_dir_compares_raw_then_canonical() {
        // Identical paths match outright (the raw short-circuit).
        assert!(same_dir(Path::new("/a/b"), Path::new("/a/b")));

        let dir = tempfile::tempdir().unwrap();
        let real = dir.path();
        // Raw-different but canonically-equal paths match via canonicalization. A
        // `sub/..` round-trip stays distinct as a `Path` (unlike a trailing `.`,
        // which `Path` normalizes away) yet canonicalizes back to `real`.
        std::fs::create_dir_all(real.join("sub")).unwrap();
        let round_trip = real.join("sub").join("..");
        assert_ne!(real, round_trip.as_path());
        assert!(same_dir(real, &round_trip));

        // Two distinct real directories canonicalize to different paths → no match
        // (both canonicalize, the guard is evaluated and fails).
        let other = tempfile::tempdir().unwrap();
        assert!(!same_dir(real, other.path()));

        // A path that cannot be canonicalized (does not exist) and is raw-different
        // also does not match.
        assert!(!same_dir(real, Path::new("/nonexistent/xyz")));
    }
}

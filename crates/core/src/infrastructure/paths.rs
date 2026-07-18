//! Where usagi keeps its data on disk, in one place.
//!
//! Two independent locations, kept here so no layer re-spells them as literals:
//!
//! - **Per-repository metadata** at `<repo>/.usagi` ([`STATE_DIR`]): the issue /
//!   memory stores and the `.gitignore` writer join it. Lives next to the code
//!   it describes and is committed with it.
//! - **The global per-user data directory** ([`data_dir`]): the production
//!   `$USAGI_HOME` / `~/.usagi`, or its `develop/` child for a debug build.
//!   The channel split prevents `cargo run` from touching a released daemon's
//!   endpoint, record, or owned state.
//!
//! The two share the `.usagi` basename by convention but are independent
//! directories with different contents and lifetimes.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// The repository-relative directory holding usagi's per-project metadata
/// (`issues/`, `memory/`, `state.json`, …): `<repo>/.usagi`.
pub const STATE_DIR: &str = ".usagi";

/// The directory under [`STATE_DIR`] that holds session worktrees, one per
/// session: `<repo>/.usagi/sessions/<name>`.
pub const SESSIONS_DIR: &str = "sessions";

/// Environment variable that overrides the default data directory.
pub const DATA_DIR_ENV: &str = "USAGI_HOME";
/// Directory created under the user's home directory by default.
const DATA_DIR_NAME: &str = ".usagi";

/// The build channel used to isolate development runtime state from releases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildChannel {
    Production,
    Development,
}

/// Returns the compile-time channel for this binary. `cargo run --release`
/// selects production because debug assertions are disabled in that profile.
#[must_use]
#[coverage(off)] // The production variant is only compiled by a release build.
pub const fn build_channel() -> BuildChannel {
    if cfg!(debug_assertions) {
        BuildChannel::Development
    } else {
        BuildChannel::Production
    }
}

/// Resolve the directory where usagi stores its per-user data.
///
/// `$USAGI_HOME` takes precedence; otherwise `~/.usagi` is used as the base.
/// Debug builds append `develop/` to that base while release builds retain
/// the base itself, preserving the established production location.
///
/// # Errors
///
/// Returns an error when `$USAGI_HOME` is unset and the home directory cannot be
/// determined.
#[coverage(off)] // The production base branch is only reachable in a release build.
pub fn data_dir() -> Result<PathBuf> {
    let base = if let Some(dir) = std::env::var_os(DATA_DIR_ENV).filter(|v| !v.is_empty()) {
        PathBuf::from(dir)
    } else {
        dirs::home_dir()
            .context("could not determine the home directory")?
            .join(DATA_DIR_NAME)
    };
    Ok(match build_channel() {
        BuildChannel::Production => base,
        BuildChannel::Development => base.join("develop"),
    })
}

#[cfg(test)]
#[coverage(off)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_prefers_env_override_then_falls_back() {
        // Serialize $USAGI_HOME mutation against other globals-mutating tests.
        let _guard = crate::test_support::process_env_guard();
        unsafe {
            std::env::set_var(DATA_DIR_ENV, "/tmp/usagi-unit-home");
        }
        let expected = match build_channel() {
            BuildChannel::Production => PathBuf::from("/tmp/usagi-unit-home"),
            BuildChannel::Development => PathBuf::from("/tmp/usagi-unit-home/develop"),
        };
        assert_eq!(data_dir().unwrap(), expected);

        // An empty override is ignored in favour of the home-directory default.
        unsafe {
            std::env::set_var(DATA_DIR_ENV, "");
        }
        assert!(data_dir().unwrap().to_string_lossy().contains(".usagi"));

        unsafe {
            std::env::remove_var(DATA_DIR_ENV);
        }
        assert!(data_dir().unwrap().to_string_lossy().contains(".usagi"));
    }

    #[test]
    fn build_channel_variants_are_distinct() {
        assert_ne!(BuildChannel::Production, BuildChannel::Development);
        assert_eq!(BuildChannel::Production.clone(), BuildChannel::Production);
        assert_eq!(format!("{:?}", BuildChannel::Production), "Production");
        assert_eq!(format!("{:?}", BuildChannel::Development), "Development");
    }
}

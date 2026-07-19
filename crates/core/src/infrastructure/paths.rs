//! Where usagi keeps its data on disk, in one place.
//!
//! Two independent locations, kept here so no layer re-spells them as literals:
//!
//! - **Per-repository metadata** at `<repo>/.usagi` ([`STATE_DIR`]): the issue /
//!   memory stores and the `.gitignore` writer join it. Lives next to the code
//!   it describes and is committed with it.
//! - **The global per-user data directory** ([`data_dir`]): the production
//!   `$USAGI_HOME` / `~/.usagi`, or its `dev/` child for a debug build.
//!   The channel split prevents `cargo run` from touching a released daemon's
//!   endpoint, record, or owned state.
//!
//! The two share the `.usagi` basename by convention but are independent
//! directories with different contents and lifetimes.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The repository-relative directory holding usagi's per-project metadata.
pub const STATE_DIR: &str = ".usagi";

/// The single directory name used for debug runtime state.
pub const DEV_DIR: &str = "dev";

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
/// Debug builds append [`DEV_DIR`] to that base while release builds retain the
/// base itself, preserving the established production location.
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
    Ok(channel_data_dir(base))
}

/// Resolve the build-channel-specific directory rooted at `base`.
///
/// Debug builds use `base/dev`; release builds use `base` unchanged. This is
/// shared by global and project-local runtime state so their channel split
/// cannot drift.
#[must_use]
pub fn channel_data_dir(base: impl AsRef<Path>) -> PathBuf {
    let base = base.as_ref();
    match build_channel() {
        BuildChannel::Production => base.to_path_buf(),
        BuildChannel::Development => base.join(DEV_DIR),
    }
}

/// Resolve the build-channel-specific runtime-state directory for a project.
///
/// Debug builds use `<project_root>/.usagi/dev`; release builds use
/// `<project_root>/.usagi`.
#[must_use]
pub fn project_data_dir(project_root: impl AsRef<Path>) -> PathBuf {
    channel_data_dir(project_root.as_ref().join(STATE_DIR))
}

#[cfg(test)]
#[coverage(off)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_prefers_env_override_then_falls_back() {
        // Serialize $USAGI_HOME mutation against other globals-mutating tests.
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(DATA_DIR_ENV, home.path()) };
        let expected = match build_channel() {
            BuildChannel::Production => home.path().to_path_buf(),
            BuildChannel::Development => home.path().join(DEV_DIR),
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
    fn project_data_dir_uses_the_same_dev_directory_definition() {
        let expected = match build_channel() {
            BuildChannel::Production => PathBuf::from("/project/.usagi"),
            BuildChannel::Development => PathBuf::from("/project/.usagi/dev"),
        };
        assert_eq!(project_data_dir("/project"), expected);
    }

    #[test]
    fn build_channel_variants_are_distinct() {
        assert_ne!(BuildChannel::Production, BuildChannel::Development);
        assert_eq!(BuildChannel::Production.clone(), BuildChannel::Production);
        assert_eq!(format!("{:?}", BuildChannel::Production), "Production");
        assert_eq!(format!("{:?}", BuildChannel::Development), "Development");
    }
}

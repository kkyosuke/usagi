//! Where usagi keeps its data on disk, in one place.
//!
//! Two independent locations, kept here so no layer re-spells them as literals:
//!
//! - **Per-repository metadata** at `<repo>/.usagi` ([`STATE_DIR`]): the issue /
//!   memory stores and the `.gitignore` writer join it. Lives next to the code
//!   it describes and is committed with it.
//! - **The global per-user data directory** ([`data_dir`]): `$USAGI_HOME` /
//!   `~/.usagi` for production, or its selected `dev/` / `local/` child for
//!   development and local use. The mode split prevents those
//!   non-production uses from touching production state.
//!
//! The two share the `.usagi` basename by convention but are independent
//! directories with different contents and lifetimes.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The repository-relative directory holding usagi's per-project metadata.
pub const STATE_DIR: &str = ".usagi";

/// The directory name used by development runtime state.
pub const DEV_DIR: &str = "dev";
/// The directory name used by local runtime state.
pub const LOCAL_DIR: &str = "local";

/// The directory under [`STATE_DIR`] that holds session worktrees, one per
/// session: `<repo>/.usagi/sessions/<name>`.
pub const SESSIONS_DIR: &str = "sessions";

/// Environment variable that overrides the default data directory.
pub const DATA_DIR_ENV: &str = "USAGI_HOME";
/// Environment variable selecting the isolated runtime state mode.
pub const RUNTIME_MODE_ENV: &str = "USAGI_RUNTIME_MODE";
/// Directory created under the user's home directory by default.
const DATA_DIR_NAME: &str = ".usagi";

/// The runtime mode used to isolate production, development, and local state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeMode {
    /// Production state, stored directly in the base data directory.
    Production,
    /// Local development state, stored below the `dev/` child directory.
    Development,
    /// Local state, stored below the `local/` child directory.
    Local,
}

/// Returns the selected runtime mode.
///
/// [`RUNTIME_MODE_ENV`] accepts `production`, `development`, and `local`.
/// When it is absent (or invalid), local is the safe default for every
/// build profile; production requires an explicit selection.
#[must_use]
pub fn runtime_mode() -> RuntimeMode {
    match std::env::var(RUNTIME_MODE_ENV).as_deref() {
        Ok("production") => RuntimeMode::Production,
        Ok("development") => RuntimeMode::Development,
        _ => RuntimeMode::Local,
    }
}

/// Resolve the directory where usagi stores its per-user data.
///
/// `$USAGI_HOME` takes precedence; otherwise `~/.usagi` is used as the base.
/// Both runtime modes append their own child directory to that base.
///
/// # Errors
///
/// Returns an error when `$USAGI_HOME` is unset and the home directory cannot be
/// determined.
pub fn data_dir() -> Result<PathBuf> {
    let base = if let Some(dir) = std::env::var_os(DATA_DIR_ENV).filter(|v| !v.is_empty()) {
        PathBuf::from(dir)
    } else {
        dirs::home_dir()
            .context("could not determine the home directory")?
            .join(DATA_DIR_NAME)
    };
    Ok(mode_data_dir(&base, runtime_mode()))
}

/// Resolve the selected-mode directory rooted at `base`.
///
/// Production mode uses `base`; development mode uses `base/dev`; local mode
/// uses `base/local`. This is shared by global and project-local runtime state
/// so their split cannot drift.
#[must_use]
pub fn channel_data_dir(base: impl AsRef<Path>) -> PathBuf {
    mode_data_dir(base.as_ref(), runtime_mode())
}

fn mode_data_dir(base: &Path, mode: RuntimeMode) -> PathBuf {
    match mode {
        RuntimeMode::Production => base.to_path_buf(),
        RuntimeMode::Local => base.join(LOCAL_DIR),
        RuntimeMode::Development => base.join(DEV_DIR),
    }
}

/// Resolve the selected-mode runtime-state directory for a project.
///
/// Production mode uses `<project_root>/.usagi`; development mode uses
/// `<project_root>/.usagi/dev`; local mode uses
/// `<project_root>/.usagi/local`.
#[must_use]
pub fn project_data_dir(project_root: impl AsRef<Path>) -> PathBuf {
    channel_data_dir(project_root.as_ref().join(STATE_DIR))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_prefers_env_override_then_falls_back() {
        // Serialize $USAGI_HOME mutation against other globals-mutating tests.
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(DATA_DIR_ENV, home.path()) };
        let expected = channel_data_dir(home.path());
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
    fn project_data_dir_uses_the_selected_mode_definition() {
        let expected = channel_data_dir("/project/.usagi");
        assert_eq!(project_data_dir("/project"), expected);
    }

    #[test]
    fn mode_data_dir_separates_all_runtime_modes() {
        let base = Path::new("/data");
        assert_eq!(
            mode_data_dir(base, RuntimeMode::Production),
            PathBuf::from("/data")
        );
        assert_eq!(
            mode_data_dir(base, RuntimeMode::Local),
            PathBuf::from("/data/local")
        );
        assert_eq!(
            mode_data_dir(base, RuntimeMode::Development),
            PathBuf::from("/data/dev")
        );
    }

    #[test]
    fn runtime_mode_variants_are_distinct() {
        assert_ne!(RuntimeMode::Production, RuntimeMode::Local);
        assert_ne!(RuntimeMode::Production, RuntimeMode::Development);
        assert_ne!(RuntimeMode::Local, RuntimeMode::Development);
        assert_eq!(RuntimeMode::Local, RuntimeMode::Local);
        assert_eq!(format!("{:?}", RuntimeMode::Local), "Local");
        assert_eq!(format!("{:?}", RuntimeMode::Development), "Development");
        assert_eq!(format!("{:?}", RuntimeMode::Production), "Production");
    }

    #[test]
    fn runtime_mode_env_explicitly_selects_each_mode() {
        let _guard = crate::test_support::process_env_guard();
        unsafe { std::env::set_var(RUNTIME_MODE_ENV, "production") };
        assert_eq!(runtime_mode(), RuntimeMode::Production);

        unsafe { std::env::set_var(RUNTIME_MODE_ENV, "local") };
        assert_eq!(runtime_mode(), RuntimeMode::Local);

        unsafe { std::env::set_var(RUNTIME_MODE_ENV, "development") };
        assert_eq!(runtime_mode(), RuntimeMode::Development);

        unsafe { std::env::set_var(RUNTIME_MODE_ENV, "invalid") };
        assert_eq!(runtime_mode(), RuntimeMode::Local);
        unsafe { std::env::remove_var(RUNTIME_MODE_ENV) };
        assert_eq!(runtime_mode(), RuntimeMode::Local);
    }
}

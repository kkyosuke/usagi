//! Where usagi keeps its data on disk, in one place.
//!
//! Two independent locations, kept here so no layer re-spells them as literals:
//!
//! - **Per-repository metadata** at `<repo>/.usagi` ([`STATE_DIR`]): the issue /
//!   memory stores and the `.gitignore` writer join it. Lives next to the code
//!   it describes and is committed with it.
//! - **The global per-user data directory** ([`data_dir`]): the selected
//!   `dev/` or `device/` child of `$USAGI_HOME` / `~/.usagi`. The mode split
//!   prevents local development and device validation from touching each other
//!   or an existing usagi installation.
//!
//! The two share the `.usagi` basename by convention but are independent
//! directories with different contents and lifetimes.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The repository-relative directory holding usagi's per-project metadata.
pub const STATE_DIR: &str = ".usagi";

/// The directory name used by development runtime state.
pub const DEV_DIR: &str = "dev";
/// The directory name used by device-validation runtime state.
pub const DEVICE_DIR: &str = "device";

/// The directory under [`STATE_DIR`] that holds session worktrees, one per
/// session: `<repo>/.usagi/sessions/<name>`.
pub const SESSIONS_DIR: &str = "sessions";

/// Environment variable that overrides the default data directory.
pub const DATA_DIR_ENV: &str = "USAGI_HOME";
/// Environment variable selecting the isolated runtime state mode.
pub const RUNTIME_MODE_ENV: &str = "USAGI_RUNTIME_MODE";
/// Directory created under the user's home directory by default.
const DATA_DIR_NAME: &str = ".usagi";

/// The runtime mode used to isolate local development from device validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeMode {
    /// Local development state, stored below the `dev/` child directory.
    Development,
    /// State used while validating the real executable on a device.
    Device,
}

/// Returns the selected runtime mode.
///
/// [`RUNTIME_MODE_ENV`] accepts `development` and `device`. When it is absent
/// (or invalid), debug builds default to development and release builds default
/// to device, preserving the existing safe separation.
#[must_use]
pub fn runtime_mode() -> RuntimeMode {
    match std::env::var(RUNTIME_MODE_ENV).as_deref() {
        Ok("development") => RuntimeMode::Development,
        Ok("device") => RuntimeMode::Device,
        _ => {
            #[cfg(debug_assertions)]
            {
                RuntimeMode::Development
            }
            #[cfg(not(debug_assertions))]
            {
                RuntimeMode::Device
            }
        }
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
/// Development mode uses `base/dev`; device mode uses `base/device`. This is
/// shared by global and project-local runtime state so their split cannot drift.
#[must_use]
pub fn channel_data_dir(base: impl AsRef<Path>) -> PathBuf {
    mode_data_dir(base.as_ref(), runtime_mode())
}

fn mode_data_dir(base: &Path, mode: RuntimeMode) -> PathBuf {
    match mode {
        RuntimeMode::Device => base.join(DEVICE_DIR),
        RuntimeMode::Development => base.join(DEV_DIR),
    }
}

/// Resolve the selected-mode runtime-state directory for a project.
///
/// Development mode uses `<project_root>/.usagi/dev`; device mode uses
/// `<project_root>/.usagi/device`.
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
    fn project_data_dir_uses_the_same_dev_directory_definition() {
        let expected = channel_data_dir("/project/.usagi");
        assert_eq!(project_data_dir("/project"), expected);
    }

    #[test]
    fn mode_data_dir_separates_both_runtime_modes() {
        let base = Path::new("/data");
        assert_eq!(
            mode_data_dir(base, RuntimeMode::Device),
            PathBuf::from("/data/device")
        );
        assert_eq!(
            mode_data_dir(base, RuntimeMode::Development),
            PathBuf::from("/data/dev")
        );
    }

    #[test]
    fn runtime_mode_variants_are_distinct() {
        assert_ne!(RuntimeMode::Device, RuntimeMode::Development);
        assert_eq!(RuntimeMode::Device, RuntimeMode::Device);
        assert_eq!(format!("{:?}", RuntimeMode::Device), "Device");
        assert_eq!(format!("{:?}", RuntimeMode::Development), "Development");
    }

    #[test]
    fn runtime_mode_env_explicitly_selects_development_or_device() {
        let _guard = crate::test_support::process_env_guard();
        let previous = std::env::var_os(RUNTIME_MODE_ENV);
        unsafe { std::env::set_var(RUNTIME_MODE_ENV, "device") };
        assert_eq!(runtime_mode(), RuntimeMode::Device);

        unsafe { std::env::set_var(RUNTIME_MODE_ENV, "development") };
        assert_eq!(runtime_mode(), RuntimeMode::Development);

        unsafe { std::env::set_var(RUNTIME_MODE_ENV, "invalid") };
        assert_eq!(
            runtime_mode(),
            if cfg!(debug_assertions) {
                RuntimeMode::Development
            } else {
                RuntimeMode::Device
            }
        );

        if let Some(value) = previous {
            unsafe { std::env::set_var(RUNTIME_MODE_ENV, value) };
        } else {
            unsafe { std::env::remove_var(RUNTIME_MODE_ENV) };
        }
    }
}

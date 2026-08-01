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

use std::ffi::OsStr;
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

/// The directory under [`STATE_DIR`] holding the workspace-scoped daemon fence.
///
/// It is a sibling of the runtime-mode children rather than a child of one,
/// because the workspace it guards is shared by every mode. Unlike [`STATE_DIR`]
/// itself — user-visible project metadata — this directory is daemon-private
/// (`0700`), which is what lets the fence node reuse the private lock-node
/// contract. See [`workspace_fence_path`].
pub const WORKSPACE_FENCE_DIR: &str = "daemon";

/// The workspace-scoped single-daemon fence node's file name.
pub const WORKSPACE_FENCE_FILE: &str = "daemon.lock";

/// Environment variable that overrides the default data directory.
pub const DATA_DIR_ENV: &str = "USAGI_HOME";
/// Environment variable selecting the isolated runtime state mode.
pub const RUNTIME_MODE_ENV: &str = "USAGI_RUNTIME_MODE";
/// Trusted workspace root forwarded to a daemon-provisioned MCP child.
pub const WORKSPACE_ROOT_ENV: &str = "USAGI_WORKSPACE_ROOT";
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

impl RuntimeMode {
    /// The [`RUNTIME_MODE_ENV`] spelling that selects this mode again.
    ///
    /// A child process re-applies the mode from this value, so it is the wire
    /// half of the [`DataHome`] contract: `base` plus this spelling has to land
    /// the child on the same directory its parent selected.
    #[must_use]
    pub fn as_env_value(self) -> &'static str {
        match self {
            Self::Production => "production",
            Self::Development => "development",
            Self::Local => "local",
        }
    }

    /// The mode an [`RUNTIME_MODE_ENV`] value selects.
    ///
    /// An absent or unrecognised value selects local, the safe default for
    /// every build profile; production requires an explicit selection.
    #[must_use]
    pub fn from_env_value(value: Option<&str>) -> Self {
        match value {
            Some("production") => Self::Production,
            Some("development") => Self::Development,
            _ => Self::Local,
        }
    }

    /// The directory this mode appends to its base, if any.
    ///
    /// Production appends nothing — its selected directory *is* the base. That
    /// asymmetry is the whole reason [`DataHome`] exists.
    #[must_use]
    fn child_dir(self) -> Option<&'static str> {
        match self {
            Self::Production => None,
            Self::Local => Some(LOCAL_DIR),
            Self::Development => Some(DEV_DIR),
        }
    }
}

/// Returns the selected runtime mode.
///
/// [`RUNTIME_MODE_ENV`] accepts `production`, `development`, and `local`.
/// When it is absent (or invalid), local is the safe default for every
/// build profile; production requires an explicit selection.
#[must_use]
pub fn runtime_mode() -> RuntimeMode {
    RuntimeMode::from_env_value(std::env::var(RUNTIME_MODE_ENV).ok().as_deref())
}

/// The two directories one runtime mode relates: the mode-neutral `base` that a
/// child process receives as [`DATA_DIR_ENV`], and the mode-`selected`
/// directory below it that this process reads and writes.
///
/// This pair is the single place the relation is decided. A caller that holds
/// only one of the two and re-derives the other by path surgery gets production
/// wrong: production selects the base *itself*, so "strip one component to get
/// the base" walks one level **above** it — into the user's home directory —
/// and that stray path then becomes a child's data home, its settings source,
/// and a sandbox writable root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataHome {
    base: PathBuf,
    mode: RuntimeMode,
}

impl DataHome {
    /// Pair a mode-neutral base directory with the mode selected over it.
    #[must_use]
    pub fn new(base: impl Into<PathBuf>, mode: RuntimeMode) -> Self {
        Self {
            base: base.into(),
            mode,
        }
    }

    /// Recover the pair from an already mode-selected directory.
    ///
    /// This is the exact inverse of [`Self::selected`]: it strips only the
    /// directory the mode itself appends, and only when `selected` really ends
    /// with it. Anything else is taken to be its own base, so the result never
    /// climbs above the directory it was handed.
    #[must_use]
    pub fn from_selected(selected: impl AsRef<Path>, mode: RuntimeMode) -> Self {
        let selected = selected.as_ref();
        let base = mode
            .child_dir()
            .filter(|child| selected.file_name() == Some(OsStr::new(child)))
            .and_then(|_| selected.parent())
            .unwrap_or(selected);
        Self::new(base, mode)
    }

    /// The mode-neutral base. A child re-applies [`Self::mode`] to it.
    #[must_use]
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// The directory this mode selects below [`Self::base`].
    #[must_use]
    pub fn selected(&self) -> PathBuf {
        mode_data_dir(&self.base, self.mode)
    }

    /// The runtime mode that relates the two directories.
    #[must_use]
    pub fn mode(&self) -> RuntimeMode {
        self.mode
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
    mode.child_dir()
        .map_or_else(|| base.to_path_buf(), |child| base.join(child))
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

/// Resolve the workspace-scoped daemon fence node for `workspace_root`:
/// `<workspace_root>/.usagi/daemon/daemon.lock`.
///
/// This deliberately does **not** go through [`channel_data_dir`]: the fence
/// guards the workspace's physical resources (its git worktrees, branches, and
/// session names), which every runtime mode shares. Placing it under a
/// mode-selected child would let `production` and `local` each take their own
/// lock over the same worktrees.
#[must_use]
pub fn workspace_fence_path(workspace_root: impl AsRef<Path>) -> PathBuf {
    workspace_root
        .as_ref()
        .join(STATE_DIR)
        .join(WORKSPACE_FENCE_DIR)
        .join(WORKSPACE_FENCE_FILE)
}

/// Resolve `candidate` to the workspace identity the daemon fences on.
///
/// Canonicalization only settles *spelling*: it collapses `.`, `..`, a trailing
/// separator, and symlinked ancestors (on macOS, the `/tmp` → `/private/tmp`
/// firmlink) onto one path, so two daemons cannot address the same workspace by
/// two names. Exclusion itself comes from the fence node's inode, which no
/// spelling can duplicate.
///
/// # Errors
///
/// Returns an error when `candidate` cannot be resolved (it does not exist, or a
/// component is not traversable).
pub fn canonical_workspace_root(candidate: impl AsRef<Path>) -> Result<PathBuf> {
    let candidate = candidate.as_ref();
    std::fs::canonicalize(candidate).with_context(|| {
        format!(
            "could not resolve the workspace root {}",
            candidate.display()
        )
    })
}

/// Spell a workspace root for the IPC workspace fence.
///
/// The fence compares an absolute canonical path, so a root that cannot be
/// spelled as absolute UTF-8 is returned empty instead of lossily converted:
/// two distinct non-UTF-8 roots can share one lossy spelling, and an empty root
/// is refused by both peers rather than matched by accident.
#[must_use]
pub fn wire_workspace_root(root: impl AsRef<Path>) -> String {
    let root = root.as_ref();
    root.to_str()
        .filter(|_| root.is_absolute())
        .map(str::to_owned)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_workspace_root_keeps_absolute_utf8_roots_and_empties_the_rest() {
        assert_eq!(wire_workspace_root("/project/root"), "/project/root");
        // A relative root cannot be compared against the daemon's canonical
        // root, so it fails closed as an empty spelling.
        assert_eq!(wire_workspace_root("project/root"), "");
        assert_eq!(wire_workspace_root(""), "");

        #[cfg(unix)]
        {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt;
            let invalid = PathBuf::from(OsString::from_vec(b"/project/\xff".to_vec()));
            assert_eq!(wire_workspace_root(invalid), "");
        }
    }

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
    fn workspace_fence_path_ignores_the_runtime_mode() {
        let _guard = crate::test_support::process_env_guard();
        let expected = PathBuf::from("/project/.usagi/daemon/daemon.lock");
        for mode in ["production", "development", "local", "bogus"] {
            unsafe { std::env::set_var(RUNTIME_MODE_ENV, mode) };
            // Every mode must land on the same node: the workspace's worktrees
            // are shared, so a mode-scoped fence would not exclude anything.
            assert_eq!(workspace_fence_path("/project"), expected);
        }
        unsafe { std::env::remove_var(RUNTIME_MODE_ENV) };
        assert_eq!(workspace_fence_path("/project"), expected);
        assert_ne!(
            workspace_fence_path("/project"),
            project_data_dir("/project")
        );
    }

    #[test]
    fn canonical_workspace_root_collapses_spellings_and_reports_missing_paths() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let nested = root.path().join("workspace");
        std::fs::create_dir(&nested).unwrap();
        let canonical = canonical_workspace_root(&nested).unwrap();

        // A trailing separator, a `.` component, and a `..` round trip all name
        // the same workspace.
        assert_eq!(
            canonical_workspace_root(nested.join(".")).unwrap(),
            canonical
        );
        assert_eq!(
            canonical_workspace_root(nested.join("..").join("workspace")).unwrap(),
            canonical
        );

        // So does reaching it through a symlinked ancestor.
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&nested, &link).unwrap();
        assert_eq!(canonical_workspace_root(&link).unwrap(), canonical);

        let error = canonical_workspace_root(nested.join("absent")).unwrap_err();
        assert!(
            format!("{error:#}").contains("could not resolve the workspace root"),
            "{error:#}"
        );
    }

    #[test]
    fn project_data_dir_uses_the_selected_mode_definition() {
        let _guard = crate::test_support::process_env_guard();
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
    fn data_home_round_trips_the_base_through_every_mode() {
        for (mode, selected) in [
            (RuntimeMode::Production, "/data"),
            (RuntimeMode::Local, "/data/local"),
            (RuntimeMode::Development, "/data/dev"),
        ] {
            let home = DataHome::new("/data", mode);
            assert_eq!(home.base(), Path::new("/data"));
            assert_eq!(home.mode(), mode);
            assert_eq!(home.selected(), PathBuf::from(selected));

            // The daemon only ever holds the selected directory, so recovering
            // the base from it must be exact — production included, where the
            // two are the same directory.
            let recovered = DataHome::from_selected(selected, mode);
            assert_eq!(recovered, home);
            assert_eq!(recovered.base(), Path::new("/data"));
            assert_ne!(recovered.base(), Path::new("/"));
        }
    }

    #[test]
    fn data_home_from_selected_never_climbs_above_what_it_was_given() {
        // A selected directory that does not carry the mode's own child is its
        // own base: stripping a component here would leak the parent.
        for mode in [
            RuntimeMode::Production,
            RuntimeMode::Local,
            RuntimeMode::Development,
        ] {
            assert_eq!(
                DataHome::from_selected("/home/dev/.usagi", mode).base(),
                Path::new("/home/dev/.usagi")
            );
        }
        // Only the matching mode's child is stripped.
        assert_eq!(
            DataHome::from_selected("/data/local", RuntimeMode::Development).base(),
            Path::new("/data/local")
        );
        // A base whose own name collides with the child directory still round
        // trips, because only the trailing component is removed.
        assert_eq!(
            DataHome::from_selected("/data/local/local", RuntimeMode::Local).base(),
            Path::new("/data/local")
        );
    }

    #[test]
    fn runtime_mode_env_values_round_trip() {
        for mode in [
            RuntimeMode::Production,
            RuntimeMode::Local,
            RuntimeMode::Development,
        ] {
            assert_eq!(RuntimeMode::from_env_value(Some(mode.as_env_value())), mode);
        }
        assert_eq!(RuntimeMode::from_env_value(None), RuntimeMode::Local);
        assert_eq!(
            RuntimeMode::from_env_value(Some("bogus")),
            RuntimeMode::Local
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

//! Repository-relative names that define usagi's managed workspace namespace.

/// The repository-relative directory holding usagi project metadata.
pub const STATE_DIR: &str = ".usagi";

/// The directory under [`STATE_DIR`] containing managed session worktrees.
pub const SESSIONS_DIR: &str = "sessions";

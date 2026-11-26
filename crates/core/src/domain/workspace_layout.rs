//! Repository-relative names that define usagi's managed workspace namespace.

/// The repository-relative directory holding usagi project metadata.
pub const STATE_DIR: &str = ".usagi";

/// The directory under [`STATE_DIR`] containing managed session worktrees.
pub const SESSIONS_DIR: &str = "sessions";

/// Prefix of the directory holding a content-addressed artifact version.
pub const ARTIFACT_BUNDLE_PREFIX: &str = "review-";

/// Whether a directory name identifies a published artifact bundle.
#[must_use]
pub fn is_artifact_bundle(name: &str) -> bool {
    name.strip_prefix(ARTIFACT_BUNDLE_PREFIX)
        .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|c| c.is_ascii_hexdigit()))
}

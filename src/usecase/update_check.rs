//! Decide whether a newer release of usagi is available than the running build.
//!
//! The released versions are the `vX.Y.Z` tags on the project's git remote (the
//! release workflow tags every published version). This module is pure: the
//! actual network fetch is injected, so the parsing and the "is it newer"
//! decision are fully testable offline. The thin shell-out that fetches the
//! tags lives in [`crate::infrastructure::release`].

use crate::domain::version::Version;

/// The result of an update check: the running build and the latest release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateStatus {
    /// The version of the running build (`CARGO_PKG_VERSION`).
    pub current: Version,
    /// The highest released version found on the remote.
    pub latest: Version,
}

impl UpdateStatus {
    /// Whether the latest release is newer than the running build.
    pub fn update_available(&self) -> bool {
        self.latest > self.current
    }
}

/// The highest `vX.Y.Z` tag in `git ls-remote --tags` output.
///
/// Each line looks like `<sha>\trefs/tags/v0.2.0`. Non-tag lines, the
/// `refs/tags/` prefix, peeled-tag suffixes (`^{}`), and any tag that is not a
/// version are all ignored. Returns the greatest version, or `None` when there
/// are no version tags.
pub fn latest_tag(ls_remote_stdout: &str) -> Option<Version> {
    ls_remote_stdout
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter_map(|(_, reference)| reference.trim().strip_prefix("refs/tags/"))
        .map(|tag| tag.trim_end_matches("^{}"))
        .filter_map(Version::parse)
        .max()
}

/// Compare the `current` version string against the latest tag in
/// `ls_remote_stdout`. Returns `None` when the current version cannot be parsed
/// or the remote has no version tags.
pub fn evaluate(current: &str, ls_remote_stdout: &str) -> Option<UpdateStatus> {
    let current = Version::parse(current)?;
    let latest = latest_tag(ls_remote_stdout)?;
    Some(UpdateStatus { current, latest })
}

/// Check for a newer release, fetching the remote tags with `fetch` (injected so
/// the network IO stays out of this pure layer). Returns the status **only when
/// an update is actually available**, so a `Some` result always means "there is
/// a newer version".
pub fn check(current: &str, fetch: impl FnOnce() -> Option<String>) -> Option<UpdateStatus> {
    let stdout = fetch()?;
    let status = evaluate(current, &stdout)?;
    status.update_available().then_some(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic `git ls-remote --tags` block with a few version tags.
    const TAGS: &str = "\
deadbeef\trefs/tags/v0.0.1
cafef00d\trefs/tags/v0.1.0
0badf00d\trefs/tags/v0.2.0";

    #[test]
    fn latest_tag_picks_the_highest_version() {
        assert_eq!(latest_tag(TAGS), Version::parse("0.2.0"));
    }

    #[test]
    fn latest_tag_ignores_non_version_and_peeled_tags() {
        let stdout = "\
1\trefs/tags/release-candidate
2\trefs/tags/v1.0.0^{}
3\trefs/tags/v0.9.0
4\trefs/heads/main";
        // The peeled `v1.0.0^{}` is normalised to `v1.0.0` and wins.
        assert_eq!(latest_tag(stdout), Version::parse("1.0.0"));
    }

    #[test]
    fn latest_tag_is_none_without_version_tags() {
        assert_eq!(latest_tag(""), None);
        assert_eq!(latest_tag("abc\trefs/tags/nightly"), None);
        // A malformed line with no tab is skipped.
        assert_eq!(latest_tag("no-tab-here"), None);
    }

    #[test]
    fn evaluate_reports_current_and_latest() {
        let status = evaluate("0.0.1", TAGS).unwrap();
        assert_eq!(status.current, Version::parse("0.0.1").unwrap());
        assert_eq!(status.latest, Version::parse("0.2.0").unwrap());
        assert!(status.update_available());
    }

    #[test]
    fn evaluate_is_none_when_current_is_unparseable() {
        assert!(evaluate("not-a-version", TAGS).is_none());
    }

    #[test]
    fn evaluate_is_none_when_remote_has_no_tags() {
        assert!(evaluate("0.0.1", "").is_none());
    }

    #[test]
    fn update_available_is_false_when_current_is_up_to_date() {
        let status = evaluate("0.2.0", TAGS).unwrap();
        assert!(!status.update_available());
        let ahead = evaluate("1.0.0", TAGS).unwrap();
        assert!(!ahead.update_available());
    }

    #[test]
    fn check_returns_a_status_only_when_an_update_is_available() {
        let status = check("0.0.1", || Some(TAGS.to_string())).unwrap();
        assert_eq!(status.latest, Version::parse("0.2.0").unwrap());
    }

    #[test]
    fn check_is_none_when_up_to_date() {
        assert!(check("0.2.0", || Some(TAGS.to_string())).is_none());
    }

    #[test]
    fn check_is_none_when_the_fetch_fails() {
        assert!(check("0.0.1", || None).is_none());
    }

    #[test]
    fn check_is_none_when_the_remote_has_no_version_tags() {
        assert!(check("0.0.1", || Some(String::new())).is_none());
    }
}

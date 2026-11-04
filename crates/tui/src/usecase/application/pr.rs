//! Validated PR links and the browser effect boundary.

use usagi_core::domain::pr_inventory::canonicalize;

/// Opens one already validated URL using an argv-based platform adapter.
pub trait BrowserOpener: Send {
    /// Opens `url`; implementations must never invoke a shell.
    ///
    /// # Errors
    ///
    /// Returns a presentation-safe platform launch failure.
    fn open(&mut self, url: &str) -> Result<(), String>;
}

/// Accepts only the canonical HTTPS GitHub PR URL passed to `BrowserOpener`.
#[must_use]
pub fn canonical_browser_url(candidate: &str) -> Option<String> {
    canonicalize(candidate).map(|identity| identity.as_url().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_browser_urls_normalize_github_links_and_reject_shellish_values() {
        assert_eq!(
            canonical_browser_url("https://github.com/o/r/pull/7/files?x=1"),
            Some("https://github.com/o/r/pull/7".into())
        );
        assert_eq!(
            canonical_browser_url("http://github.com/o/r/pull/7"),
            Some("https://github.com/o/r/pull/7".into())
        );
        assert_eq!(
            canonical_browser_url("https://github.com/o/r/pull/7;rm -rf /"),
            None
        );
    }
}

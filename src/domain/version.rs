//! Semantic version parsing and comparison.
//!
//! Used to tell whether a newer release of usagi has been published than the
//! build that is running, so the home screen can surface an "update available"
//! notice. Only the numeric `major.minor.patch` core is compared; any
//! pre-release / build metadata in the source string is ignored.

use std::fmt;

/// A semantic version: `major.minor.patch`.
///
/// Ordering is field-by-field (major, then minor, then patch), so the derived
/// [`Ord`] gives the natural "is this release newer" comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    /// Parse a version string such as `v0.2.1`, `0.2.1`, or `0.2.1-rc.1`.
    ///
    /// A leading `v` / `V` is ignored, as is any pre-release (`-…`) or build
    /// (`+…`) suffix. A missing minor or patch component defaults to `0` (so
    /// `1` parses as `1.0.0` and `1.2` as `1.2.0`). Returns `None` when a
    /// present component is not a non-negative integer.
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim();
        let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
        // Drop any pre-release / build metadata, keeping only the numeric core.
        let core = s.split(['-', '+']).next().unwrap_or(s);
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().map_or(Some(0), |p| p.parse().ok())?;
        let patch = parts.next().map_or(Some(0), |p| p.parse().ok())?;
        Some(Version {
            major,
            minor,
            patch,
        })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_plain_version() {
        assert_eq!(Version::parse("1.2.3"), Version::parse("1.2.3"));
        let v = Version::parse("1.2.3").unwrap();
        assert_eq!(v.to_string(), "1.2.3");
    }

    #[test]
    fn ignores_a_leading_v() {
        assert_eq!(Version::parse("v0.2.1"), Version::parse("0.2.1"));
        assert_eq!(Version::parse("V0.2.1"), Version::parse("0.2.1"));
    }

    #[test]
    fn missing_components_default_to_zero() {
        assert_eq!(Version::parse("1"), Version::parse("1.0.0"));
        assert_eq!(Version::parse("1.2"), Version::parse("1.2.0"));
    }

    #[test]
    fn drops_pre_release_and_build_metadata() {
        assert_eq!(Version::parse("1.2.3-rc.1"), Version::parse("1.2.3"));
        assert_eq!(Version::parse("1.2.3+build.5"), Version::parse("1.2.3"));
    }

    #[test]
    fn rejects_non_numeric_components() {
        assert!(Version::parse("").is_none());
        assert!(Version::parse("v").is_none());
        assert!(Version::parse("abc").is_none());
        assert!(Version::parse("1.x.0").is_none());
        assert!(Version::parse("1.2.z").is_none());
    }

    #[test]
    fn orders_by_major_then_minor_then_patch() {
        let v = |s| Version::parse(s).unwrap();
        assert!(v("1.0.0") > v("0.9.9"));
        assert!(v("0.2.0") > v("0.1.9"));
        assert!(v("0.0.2") > v("0.0.1"));
        assert!(v("0.0.1") == v("0.0.1"));
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(Version::parse("  1.2.3  "), Version::parse("1.2.3"));
    }
}

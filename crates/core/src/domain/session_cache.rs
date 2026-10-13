//! Project-selected, regenerable session directories and their retention policy.

use std::path::{Component, PathBuf};

use serde::Deserialize;

/// An empty path list disables automatic cleanup. Limits are workspace-wide;
/// live sessions may keep usage above the configured soft size limit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionCachePolicy {
    pub paths: Vec<PathBuf>,
    pub max_age_hours: Option<u64>,
    pub max_bytes: Option<u64>,
}

impl SessionCachePolicy {
    /// Reject ambiguous, overlapping, or protected paths before any filesystem IO.
    ///
    /// # Errors
    /// Returns a reason when the policy could name source or management data.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.paths.is_empty() && self.max_age_hours.is_none() && self.max_bytes.is_none() {
            return Err("cache_cleanup requires max_age_hours or max_bytes");
        }
        for (index, path) in self.paths.iter().enumerate() {
            if path.as_os_str().is_empty()
                || path.components().any(|component| match component {
                    Component::Normal(name) => {
                        name.to_string_lossy().eq_ignore_ascii_case(".git")
                            || name.to_string_lossy().eq_ignore_ascii_case(".usagi")
                            || name.to_string_lossy().chars().any(char::is_control)
                    }
                    _ => true,
                })
            {
                return Err(
                    "cache_cleanup paths must be relative directories outside .git and .usagi",
                );
            }
            if self.paths[..index]
                .iter()
                .any(|other| path.starts_with(other) || other.starts_with(path))
            {
                return Err("cache_cleanup paths must not overlap");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::SessionCachePolicy;

    #[test]
    fn cache_policy_rejects_protected_and_overlapping_paths() {
        assert_eq!(SessionCachePolicy::default().validate(), Ok(()));
        let mut policy = SessionCachePolicy {
            paths: vec!["target".into()],
            max_bytes: Some(10),
            max_age_hours: None,
        };
        assert_eq!(policy.validate(), Ok(()));
        for path in [
            "",
            "/tmp",
            "..",
            "target/..",
            ".",
            ".git",
            ".usagi",
            "nested/.git",
            ".USAGI",
            "nested/.Git",
            "cache\n",
        ] {
            policy.paths = vec![path.into()];
            assert!(policy.validate().is_err(), "{path:?}");
        }
        for paths in [
            ["target", "target"],
            ["target", "target/debug"],
            ["target/debug", "target"],
        ] {
            policy.paths = paths.into_iter().map(Into::into).collect();
            assert!(policy.validate().is_err());
        }
        policy.paths = vec!["node_modules".into(), "nested/.next".into()];
        assert_eq!(policy.validate(), Ok(()));
        policy.max_bytes = None;
        assert!(policy.validate().is_err());
        policy.max_age_hours = Some(24);
        assert_eq!(policy.validate(), Ok(()));
    }
}

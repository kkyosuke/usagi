//! Parsing and validation of Git repository URLs.
//!
//! Mirrors the behaviour of "clone repository" dialogs in editors such as
//! VS Code ("Clone Git Repository…") and IntelliJ ("Get from Version
//! Control"): the user types a repository URL and the target directory name
//! is derived from the last path segment of that URL.

use std::fmt;

/// A repository URL that has passed basic validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoUrl {
    raw: String,
}

/// Why a repository URL was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoUrlError {
    /// The input was empty (or only whitespace).
    Empty,
    /// The input does not look like a repository URL (no path component).
    Invalid,
}

impl fmt::Display for RepoUrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RepoUrlError::Empty => write!(f, "enter a repository URL"),
            RepoUrlError::Invalid => write!(f, "that does not look like a repository URL"),
        }
    }
}

impl std::error::Error for RepoUrlError {}

impl RepoUrl {
    /// Parse and validate a repository URL.
    ///
    /// Accepts the common Git URL shapes:
    /// - HTTPS: `https://github.com/owner/repo.git`
    /// - SSH (scp-like): `git@github.com:owner/repo.git`
    /// - SSH (URL): `ssh://git@host/owner/repo.git`
    pub fn parse(input: &str) -> Result<Self, RepoUrlError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(RepoUrlError::Empty);
        }
        // A real URL has a host/path separator and a non-empty final segment.
        let has_separator = trimmed.contains('/') || trimmed.contains(':');
        if !has_separator || final_segment(trimmed).is_none() {
            return Err(RepoUrlError::Invalid);
        }
        Ok(Self {
            raw: trimmed.to_string(),
        })
    }

    /// The validated URL as entered (whitespace trimmed).
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// The directory name a clone of this URL would create.
    pub fn directory_name(&self) -> String {
        // Guaranteed to be Some because `parse` rejected URLs without one.
        final_segment(&self.raw).unwrap_or_default()
    }
}

/// Best-effort directory-name suggestion for a possibly-incomplete URL.
///
/// Used to live-update the directory field while the user is still typing the
/// URL. A trailing slash yields `None` so the field stays empty while the user
/// is mid-way through typing the path (e.g. just after `owner/`).
pub fn suggest_directory(input: &str) -> Option<String> {
    last_segment(input.trim())
}

/// Extract the final path segment, stripping a trailing `.git` suffix. Splits
/// on `/` (path) and `:` (scp-like `git@host:owner/repo`). `None` if empty.
fn last_segment(input: &str) -> Option<String> {
    let segment = input.rsplit(['/', ':']).next().unwrap_or("");
    let segment = segment.strip_suffix(".git").unwrap_or(segment);
    if segment.is_empty() {
        None
    } else {
        Some(segment.to_string())
    }
}

/// Like [`last_segment`] but tolerant of trailing slashes — used once the URL
/// is complete so `…/repo/` still resolves to `repo`.
fn final_segment(input: &str) -> Option<String> {
    last_segment(input.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_directory_from_https_url() {
        let url = RepoUrl::parse("https://github.com/owner/repo.git").unwrap();
        assert_eq!(url.directory_name(), "repo");
    }

    #[test]
    fn derives_directory_from_https_url_without_git_suffix() {
        let url = RepoUrl::parse("https://github.com/owner/repo").unwrap();
        assert_eq!(url.directory_name(), "repo");
    }

    #[test]
    fn derives_directory_from_scp_like_ssh_url() {
        let url = RepoUrl::parse("git@github.com:owner/repo.git").unwrap();
        assert_eq!(url.directory_name(), "repo");
    }

    #[test]
    fn derives_directory_from_ssh_url() {
        let url = RepoUrl::parse("ssh://git@host.example/owner/repo.git").unwrap();
        assert_eq!(url.directory_name(), "repo");
    }

    #[test]
    fn ignores_trailing_slash() {
        let url = RepoUrl::parse("https://github.com/owner/repo/").unwrap();
        assert_eq!(url.directory_name(), "repo");
    }

    #[test]
    fn trims_surrounding_whitespace() {
        let url = RepoUrl::parse("  https://github.com/owner/repo.git  ").unwrap();
        assert_eq!(url.as_str(), "https://github.com/owner/repo.git");
        assert_eq!(url.directory_name(), "repo");
    }

    #[test]
    fn rejects_empty_input() {
        assert_eq!(RepoUrl::parse("   "), Err(RepoUrlError::Empty));
    }

    #[test]
    fn rejects_input_without_path() {
        assert_eq!(RepoUrl::parse("notaurl"), Err(RepoUrlError::Invalid));
    }

    #[test]
    fn suggests_directory_for_partial_input() {
        assert_eq!(
            suggest_directory("https://github.com/owner/re"),
            Some("re".to_string())
        );
        // A trailing slash means the path segment hasn't been typed yet.
        assert_eq!(suggest_directory("https://github.com/owner/"), None);
        assert_eq!(suggest_directory(""), None);
        assert_eq!(
            suggest_directory("https://github.com/owner/repo"),
            Some("repo".to_string())
        );
    }
}

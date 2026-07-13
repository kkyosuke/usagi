//! `usagi update` — 最新 release のバイナリをダウンロードして導入する。

use std::io::{self, Write};

use crate::cli::{Run, RunOutcome};

/// usagi の GitHub repository URL。
const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

/// `usagi update` のハンドラ。実際の subprocess は合成ルートが実行する。
pub struct Update;

impl Run for Update {
    #[coverage(off)]
    fn run(&self, out: &mut dyn Write) -> io::Result<RunOutcome> {
        let command = install_command(REPOSITORY)
            .ok_or_else(|| io::Error::other("usagi repository URL is not a GitHub URL"))?;
        writeln!(
            out,
            "downloading and installing the latest usagi release..."
        )?;
        Ok(RunOutcome::SelfUpdate { command })
    }
}

/// Build the documented installer invocation for a GitHub repository URL.
#[coverage(off)]
fn install_command(repository: &str) -> Option<String> {
    let slug = repository
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .strip_prefix("https://github.com/")?;
    Some(format!(
        "curl -fsSL https://raw.githubusercontent.com/{slug}/main/scripts/install.sh | bash"
    ))
}

#[cfg(test)]
mod tests {
    use super::{Update, install_command};
    use crate::cli::{Run, RunOutcome};

    #[test]
    fn installer_command_uses_the_release_downloading_script() {
        assert_eq!(
            install_command("https://github.com/KKyosuke/usagi.git"),
            Some("curl -fsSL https://raw.githubusercontent.com/KKyosuke/usagi/main/scripts/install.sh | bash".into())
        );
        assert_eq!(install_command("https://example.com/usagi"), None);
    }

    #[test]
    fn handler_requests_a_self_update_from_the_composition_root() {
        let mut out = Vec::new();
        let outcome = Update.run(&mut out).unwrap();
        assert!(
            matches!(outcome, RunOutcome::SelfUpdate { command } if command.contains("scripts/install.sh"))
        );
        assert!(String::from_utf8(out).unwrap().contains("downloading"));
    }
}

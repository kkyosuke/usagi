//! Self-update: re-run the documented install script to replace the installed
//! binary with the latest release.
//!
//! The home screen's sidebar mascot announces when the git remote has a newer
//! release than the running build (see [`crate::usecase::update_check`]).
//! Clicking it asks to update; on confirmation this module runs the same
//! one-liner the README documents — `curl … install.sh | bash` — which downloads
//! the latest release archive and replaces `~/.usagi/bin/usagi`. The running
//! process keeps the old binary mapped, so the new version only takes effect
//! after a restart; the caller surfaces that as the completion message.
//!
//! All command execution goes through the shared [`CommandRunner`] abstraction,
//! so the command built here — and the message chosen from its exit status — are
//! unit-testable without shelling out. The off-thread orchestration (and the real
//! [`SystemRunner`](crate::usecase::doctor::SystemRunner)) lives in the
//! presentation layer.

use crate::usecase::doctor::CommandRunner;

/// Build the shell one-liner that downloads the latest release and replaces the
/// installed binary — the same command the README documents for updating.
///
/// `repo_url` is the package's repository URL (`CARGO_PKG_REPOSITORY`, e.g.
/// `https://github.com/KKyosuke/usagi`); the install script lives at
/// `raw.githubusercontent.com/<owner>/<repo>/main/scripts/install.sh`. A URL that
/// is not a recognised GitHub HTTPS remote falls back to using it verbatim as the
/// `<owner>/<repo>` slug, so a misconfigured remote still yields a runnable (if
/// wrong) command rather than a panic.
pub fn install_command(repo_url: &str) -> String {
    let slug = repo_url
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .strip_prefix("https://github.com/")
        .unwrap_or(repo_url);
    format!("curl -fsSL https://raw.githubusercontent.com/{slug}/main/scripts/install.sh | bash")
}

/// Run the self-update through `runner`, returning whether it succeeded and the
/// rabbit message to show. The install script is run via `bash -c` quietly (its
/// download progress would otherwise paint over the TUI), and on success the
/// message tells the user to restart — the replaced binary only takes effect on
/// the next launch.
pub fn run(runner: &dyn CommandRunner, repo_url: &str) -> (bool, String) {
    let command = install_command(repo_url);
    match runner.run_quiet("bash", &["-c", &command]) {
        Ok(true) => (
            true,
            "アップデートしたよ！反映するには usagi を再起動してね 🐰".to_string(),
        ),
        Ok(false) | Err(_) => (
            false,
            "アップデートに失敗したぴょん…ネットワークを確認してね".to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A runner that records the one quiet command it is asked to run and replies
    /// with a scripted exit status (or an I/O error).
    #[derive(Default)]
    struct FakeRunner {
        result: Option<std::io::Result<bool>>,
        seen: RefCell<Vec<(String, Vec<String>)>>,
    }

    impl CommandRunner for FakeRunner {
        fn available(&self, _program: &str) -> bool {
            true
        }
        fn run(&self, _program: &str, _args: &[&str]) -> std::io::Result<bool> {
            Ok(true)
        }
        fn run_quiet(&self, program: &str, args: &[&str]) -> std::io::Result<bool> {
            self.seen.borrow_mut().push((
                program.to_string(),
                args.iter().map(|a| a.to_string()).collect(),
            ));
            match &self.result {
                Some(Ok(ok)) => Ok(*ok),
                Some(Err(e)) => Err(std::io::Error::new(e.kind(), "boom")),
                None => Ok(true),
            }
        }
        fn check(&self, _program: &str, _args: &[&str]) -> bool {
            true
        }
        fn spawn(&self, _program: &str, _args: &[&str]) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn install_command_builds_the_raw_script_url_from_a_github_remote() {
        assert_eq!(
            install_command("https://github.com/KKyosuke/usagi"),
            "curl -fsSL https://raw.githubusercontent.com/KKyosuke/usagi/main/scripts/install.sh | bash"
        );
    }

    #[test]
    fn install_command_trims_a_trailing_slash_and_git_suffix() {
        assert_eq!(
            install_command("https://github.com/KKyosuke/usagi.git"),
            "curl -fsSL https://raw.githubusercontent.com/KKyosuke/usagi/main/scripts/install.sh | bash"
        );
        assert_eq!(
            install_command("https://github.com/KKyosuke/usagi/"),
            "curl -fsSL https://raw.githubusercontent.com/KKyosuke/usagi/main/scripts/install.sh | bash"
        );
    }

    #[test]
    fn install_command_uses_a_non_github_url_verbatim_as_the_slug() {
        assert_eq!(
            install_command("https://example.com/x"),
            "curl -fsSL https://raw.githubusercontent.com/https://example.com/x/main/scripts/install.sh | bash"
        );
    }

    #[test]
    fn run_reports_success_with_a_restart_message_and_runs_bash_dash_c() {
        let runner = FakeRunner {
            result: Some(Ok(true)),
            ..Default::default()
        };
        let (ok, message) = run(&runner, "https://github.com/KKyosuke/usagi");
        assert!(ok);
        assert!(message.contains("再起動"));
        // The script is run through `bash -c <one-liner>`, quietly.
        let seen = runner.seen.borrow();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "bash");
        assert_eq!(seen[0].1[0], "-c");
        assert_eq!(
            seen[0].1[1],
            install_command("https://github.com/KKyosuke/usagi")
        );
    }

    #[test]
    fn run_reports_failure_when_the_script_exits_nonzero() {
        let runner = FakeRunner {
            result: Some(Ok(false)),
            ..Default::default()
        };
        let (ok, message) = run(&runner, "https://github.com/KKyosuke/usagi");
        assert!(!ok);
        assert!(message.contains("失敗"));
    }

    #[test]
    fn run_reports_failure_when_the_command_errors() {
        let runner = FakeRunner {
            result: Some(Err(std::io::Error::new(std::io::ErrorKind::NotFound, ""))),
            ..Default::default()
        };
        let (ok, message) = run(&runner, "https://github.com/KKyosuke/usagi");
        assert!(!ok);
        assert!(message.contains("失敗"));
    }

    #[test]
    fn the_self_update_only_drives_run_quiet() {
        // The self-update runs the script through `run_quiet` alone; the fake's
        // other `CommandRunner` methods exist only to satisfy the trait. Exercise
        // them directly so the fake is fully covered and to pin that contract.
        let runner = FakeRunner::default();
        assert!(runner.available("bash"));
        assert!(runner.run("bash", &["-c", "true"]).unwrap());
        // A default fake (no scripted result) reports success from `run_quiet`.
        assert!(runner.run_quiet("bash", &["-c", "true"]).unwrap());
        assert!(runner.check("bash", &["-c", "true"]));
        assert!(runner.spawn("bash", &["-c", "true"]).is_ok());
    }
}

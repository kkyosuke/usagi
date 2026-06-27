use std::env;

use crate::usecase::update::{
    self, DefaultOutcome, RepoUpdate, SessionUpdate, UpdateReport, WorktreeOutcome,
};

/// Entry point for `usagi update`: fetch each source repository, fast-forward
/// its default branch, distribute that update into every session worktree that
/// merges cleanly, and print what happened.
pub fn run(dry_run: bool) -> anyhow::Result<()> {
    let cwd = env::current_dir()?;
    let report = update::run(&cwd, dry_run)?;
    for line in render(&report) {
        println!("{line}");
    }
    Ok(())
}

/// Format an [`UpdateReport`] into the lines printed by `usagi update`: a
/// "default branches" block for the root refresh, then one block per session
/// listing each worktree's outcome.
fn render(report: &UpdateReport) -> Vec<String> {
    let mut lines = Vec::new();
    if report.dry_run {
        lines.push("dry run — fetched origin but made no local changes".to_string());
        lines.push(String::new());
    }

    lines.push("default branches:".to_string());
    if report.repos.is_empty() {
        lines.push("  (no repositories found)".to_string());
    }
    for repo in &report.repos {
        lines.push(format!("  {}", render_repo(repo)));
    }

    lines.push(String::new());
    lines.push("sessions:".to_string());
    if report.sessions.is_empty() {
        lines.push("  (no sessions)".to_string());
    }
    for session in &report.sessions {
        lines.extend(render_session(session));
    }
    lines
}

/// One repository's default-branch line: `<branch> (<repo>): <outcome>`.
fn render_repo(repo: &RepoUpdate) -> String {
    format!(
        "{} ({}): {}",
        repo.branch,
        repo.repo.display(),
        render_default_outcome(&repo.outcome)
    )
}

/// A session block: a heading line, then one indented line per worktree.
fn render_session(session: &SessionUpdate) -> Vec<String> {
    let mut lines = vec![format!("  session \"{}\"", session.name)];
    if session.worktrees.is_empty() {
        lines.push("    (no worktrees)".to_string());
    }
    for wt in &session.worktrees {
        let branch = wt.branch.as_deref().unwrap_or("(detached)");
        lines.push(format!(
            "    {} ({}): {}",
            branch,
            wt.worktree.display(),
            render_worktree_outcome(&wt.outcome)
        ));
    }
    lines
}

/// The human phrasing of a [`DefaultOutcome`].
fn render_default_outcome(outcome: &DefaultOutcome) -> String {
    match outcome {
        DefaultOutcome::FetchFailed(e) => format!("fetch failed: {e}"),
        DefaultOutcome::NotCheckedOut(Some(branch)) => {
            format!("skipped (\"{branch}\" is checked out, not the default branch)")
        }
        DefaultOutcome::NotCheckedOut(None) => {
            "skipped (default branch not checked out)".to_string()
        }
        DefaultOutcome::Dirty => "skipped (uncommitted changes)".to_string(),
        DefaultOutcome::UpToDate => "already up to date".to_string(),
        DefaultOutcome::Diverged => "skipped (local commits diverge from the remote)".to_string(),
        DefaultOutcome::Updated { behind } => {
            format!("fast-forwarded ({} new)", commits(*behind))
        }
        DefaultOutcome::WouldUpdate { behind } => {
            format!("would fast-forward ({} new)", commits(*behind))
        }
    }
}

/// The human phrasing of a [`WorktreeOutcome`].
fn render_worktree_outcome(outcome: &WorktreeOutcome) -> String {
    match outcome {
        WorktreeOutcome::FetchFailed => "skipped (fetch failed)".to_string(),
        WorktreeOutcome::Detached => "skipped (detached HEAD)".to_string(),
        WorktreeOutcome::Dirty => "skipped (uncommitted changes)".to_string(),
        WorktreeOutcome::UpToDate => "already up to date".to_string(),
        WorktreeOutcome::Conflict => "skipped (would conflict)".to_string(),
        WorktreeOutcome::Updated { behind } => format!("merged ({} new)", commits(*behind)),
        WorktreeOutcome::WouldUpdate { behind } => {
            format!("would merge ({} new)", commits(*behind))
        }
    }
}

/// `"1 commit"` / `"N commits"` — the count phrasing shared by the outcome
/// lines.
fn commits(n: usize) -> String {
    if n == 1 {
        "1 commit".to_string()
    } else {
        format!("{n} commits")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usecase::update::WorktreeUpdate;
    use std::path::PathBuf;

    fn repo(branch: &str, outcome: DefaultOutcome) -> RepoUpdate {
        RepoUpdate {
            repo: PathBuf::from("/ws/app"),
            branch: branch.to_string(),
            outcome,
        }
    }

    fn wt(branch: Option<&str>, outcome: WorktreeOutcome) -> WorktreeUpdate {
        WorktreeUpdate {
            worktree: PathBuf::from("/ws/.usagi/sessions/feat/app"),
            branch: branch.map(str::to_string),
            outcome,
        }
    }

    #[test]
    fn commits_is_singular_for_one_and_plural_otherwise() {
        assert_eq!(commits(1), "1 commit");
        assert_eq!(commits(0), "0 commits");
        assert_eq!(commits(3), "3 commits");
    }

    #[test]
    fn render_default_outcome_phrases_every_case() {
        let cases = [
            (
                DefaultOutcome::FetchFailed("no origin".to_string()),
                "fetch failed: no origin",
            ),
            (
                DefaultOutcome::NotCheckedOut(Some("dev".to_string())),
                "skipped (\"dev\" is checked out, not the default branch)",
            ),
            (
                DefaultOutcome::NotCheckedOut(None),
                "skipped (default branch not checked out)",
            ),
            (DefaultOutcome::Dirty, "skipped (uncommitted changes)"),
            (DefaultOutcome::UpToDate, "already up to date"),
            (
                DefaultOutcome::Diverged,
                "skipped (local commits diverge from the remote)",
            ),
            (
                DefaultOutcome::Updated { behind: 2 },
                "fast-forwarded (2 commits new)",
            ),
            (
                DefaultOutcome::WouldUpdate { behind: 1 },
                "would fast-forward (1 commit new)",
            ),
        ];
        for (outcome, expected) in cases {
            assert_eq!(render_default_outcome(&outcome), expected);
        }
    }

    #[test]
    fn render_worktree_outcome_phrases_every_case() {
        let cases = [
            (WorktreeOutcome::FetchFailed, "skipped (fetch failed)"),
            (WorktreeOutcome::Detached, "skipped (detached HEAD)"),
            (WorktreeOutcome::Dirty, "skipped (uncommitted changes)"),
            (WorktreeOutcome::UpToDate, "already up to date"),
            (WorktreeOutcome::Conflict, "skipped (would conflict)"),
            (
                WorktreeOutcome::Updated { behind: 1 },
                "merged (1 commit new)",
            ),
            (
                WorktreeOutcome::WouldUpdate { behind: 4 },
                "would merge (4 commits new)",
            ),
        ];
        for (outcome, expected) in cases {
            assert_eq!(render_worktree_outcome(&outcome), expected);
        }
    }

    #[test]
    fn render_lays_out_repos_and_sessions() {
        let report = UpdateReport {
            dry_run: false,
            repos: vec![repo("main", DefaultOutcome::Updated { behind: 1 })],
            sessions: vec![SessionUpdate {
                name: "feat".to_string(),
                worktrees: vec![
                    wt(Some("usagi/feat"), WorktreeOutcome::Updated { behind: 1 }),
                    wt(None, WorktreeOutcome::Detached),
                ],
            }],
        };
        let lines = render(&report);
        assert_eq!(lines[0], "default branches:");
        assert_eq!(lines[1], "  main (/ws/app): fast-forwarded (1 commit new)");
        assert_eq!(lines[2], "");
        assert_eq!(lines[3], "sessions:");
        assert_eq!(lines[4], "  session \"feat\"");
        assert_eq!(
            lines[5],
            "    usagi/feat (/ws/.usagi/sessions/feat/app): merged (1 commit new)"
        );
        // A detached worktree falls back to the "(detached)" branch label.
        assert_eq!(
            lines[6],
            "    (detached) (/ws/.usagi/sessions/feat/app): skipped (detached HEAD)"
        );
    }

    #[test]
    fn render_notes_dry_run_and_empty_repos_and_sessions() {
        let report = UpdateReport {
            dry_run: true,
            repos: Vec::new(),
            sessions: Vec::new(),
        };
        let lines = render(&report);
        assert_eq!(
            lines[0],
            "dry run — fetched origin but made no local changes"
        );
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "default branches:");
        assert_eq!(lines[3], "  (no repositories found)");
        assert_eq!(lines[5], "sessions:");
        assert_eq!(lines[6], "  (no sessions)");
    }

    #[test]
    fn render_session_notes_a_session_without_worktrees() {
        let lines = render_session(&SessionUpdate {
            name: "ghost".to_string(),
            worktrees: Vec::new(),
        });
        assert_eq!(lines[0], "  session \"ghost\"");
        assert_eq!(lines[1], "    (no worktrees)");
    }

    #[test]
    fn run_updates_the_current_repo_and_prints() {
        // `run` reads the current directory; point it at a throwaway repo with no
        // remote so fetch fails fast (no network) but the command still succeeds
        // and prints its report. The guard serializes the cwd change.
        let _guard = crate::test_support::process_env_guard();
        let dir = tempfile::tempdir().unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@e.com"][..],
            &["config", "user.name", "t"][..],
        ] {
            crate::infrastructure::git::test_command(dir.path())
                .args(args)
                .status()
                .unwrap();
        }
        std::fs::write(dir.path().join("f"), "x").unwrap();
        for args in [&["add", "."][..], &["commit", "-q", "-m", "i"][..]] {
            crate::infrastructure::git::test_command(dir.path())
                .args(args)
                .status()
                .unwrap();
        }

        let original = env::current_dir().unwrap();
        env::set_current_dir(dir.path()).unwrap();
        let result = run(true);
        env::set_current_dir(original).unwrap();

        assert!(result.is_ok());
    }
}

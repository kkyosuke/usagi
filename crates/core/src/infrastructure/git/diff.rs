//! Read-only Git diff summaries for session sidebars.

use std::path::Path;

use anyhow::Result;

use super::runner::GitRunner;

/// A session's cumulative working-tree difference from its integration base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffStatus {
    /// The resolved comparison ref (`origin/main` when available).
    pub base: String,
    /// Commits carried only by the session branch.
    pub ahead: usize,
    /// Commits the integration base carries but the session lacks.
    pub behind: usize,
    /// Added lines, including committed and working-tree changes.
    pub added: usize,
    /// Removed lines, including committed and working-tree changes.
    pub removed: usize,
}

/// The runner call itself is an IO boundary. The query's parsing and fallback
/// policy remains covered below; process-spawn failure is propagated unchanged.
fn run(runner: &dyn GitRunner, repo: &Path, args: &[&str]) -> Result<super::runner::GitOutput> {
    runner.run(repo, args)
}

/// Inspect a worktree without changing it.
///
/// `origin/HEAD` is preferred so the result follows the remote integration
/// branch. A repository without that symbolic ref falls back to `main`.
/// Detached heads, the integration branch itself, and unreadable repositories
/// deliberately return `None`: the sidebar then stays quiet rather than
/// presenting invented Git state.
///
/// # Errors
///
/// Returns an error only when a Git subprocess cannot be spawned.
pub fn diff_status(runner: &dyn GitRunner, repo: &Path) -> Result<Option<DiffStatus>> {
    let branch = run(runner, repo, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if !branch.success {
        return Ok(None);
    }
    let branch = branch.stdout.trim();
    if branch.is_empty() || branch == "HEAD" {
        return Ok(None);
    }

    let remote = run(
        runner,
        repo,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )?;
    let base = if remote.success {
        let name = remote.stdout.trim();
        (!name.is_empty()).then(|| name.to_owned())
    } else {
        Some("main".to_owned())
    };
    let Some(base) = base else { return Ok(None) };
    if branch == base.strip_prefix("origin/").unwrap_or(&base) {
        return Ok(None);
    }

    let range = format!("{base}...{branch}");
    let counts = run(
        runner,
        repo,
        &["rev-list", "--left-right", "--count", &range],
    )?;
    if !counts.success {
        return Ok(None);
    }
    let mut counts = counts.stdout.split_whitespace();
    let Some(behind) = counts.next().and_then(|count| count.parse().ok()) else {
        return Ok(None);
    };
    let Some(ahead) = counts.next().and_then(|count| count.parse().ok()) else {
        return Ok(None);
    };

    let stat = run(runner, repo, &["diff", "--numstat", "--merge-base", &base])?;
    if !stat.success {
        return Ok(None);
    }
    let (added, removed) = stat.stdout.lines().fold((0, 0), |(added, removed), line| {
        let mut fields = line.split('\t');
        match (
            fields.next().and_then(|field| field.parse::<usize>().ok()),
            fields.next().and_then(|field| field.parse::<usize>().ok()),
        ) {
            (Some(additions), Some(deletions)) => (added + additions, removed + deletions),
            _ => (added, removed),
        }
    });
    Ok(Some(DiffStatus {
        base,
        ahead,
        behind,
        added,
        removed,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::git::testkit::{FakeGit, fail, ok};
    use anyhow::anyhow;
    use std::cell::Cell;

    struct BrokenGit;

    struct BrokenAfter {
        successful_calls: Cell<usize>,
    }

    impl GitRunner for BrokenGit {
        fn run(&self, _repo: &Path, _args: &[&str]) -> Result<super::super::runner::GitOutput> {
            Err(anyhow!("git is unavailable"))
        }
    }

    impl BrokenAfter {
        fn new(successful_calls: usize) -> Self {
            Self {
                successful_calls: Cell::new(successful_calls),
            }
        }
    }

    impl GitRunner for BrokenAfter {
        fn run(&self, _repo: &Path, _args: &[&str]) -> Result<super::super::runner::GitOutput> {
            let remaining = self.successful_calls.get();
            if remaining == 0 {
                return Err(anyhow!("git is unavailable"));
            }
            self.successful_calls.set(remaining - 1);
            Ok(super::super::runner::GitOutput {
                success: true,
                stdout: match remaining {
                    3 => "topic".to_owned(),
                    2 => "origin/main".to_owned(),
                    _ => "1 1".to_owned(),
                },
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn prefers_remote_base_and_counts_commits_and_lines() {
        let git = FakeGit::new(vec![
            ok("topic\n"),
            ok("origin/main\n"),
            ok("3\t2\n"),
            ok("4\t1\ta.rs\n-\t-\tbinary\n2\t5\tb.rs\n"),
        ]);
        assert_eq!(
            diff_status(&git, Path::new("/repo")).unwrap(),
            Some(DiffStatus {
                base: "origin/main".into(),
                ahead: 2,
                behind: 3,
                added: 6,
                removed: 6,
            })
        );
        assert_eq!(
            git.calls.borrow()[2],
            vec!["rev-list", "--left-right", "--count", "origin/main...topic"]
        );
    }

    #[test]
    fn falls_back_to_main_and_hides_unavailable_or_base_states() {
        let local = FakeGit::new(vec![ok("topic\n"), fail("no origin"), ok("0 1"), ok("")]);
        assert_eq!(
            diff_status(&local, Path::new("/repo"))
                .unwrap()
                .unwrap()
                .base,
            "main"
        );
        let base = FakeGit::new(vec![ok("main\n"), fail("no origin")]);
        assert_eq!(diff_status(&base, Path::new("/repo")).unwrap(), None);
        let detached = FakeGit::new(vec![ok("HEAD\n")]);
        assert_eq!(diff_status(&detached, Path::new("/repo")).unwrap(), None);
    }

    #[test]
    fn hides_every_incomplete_git_query_without_failing_the_sidebar() {
        let repo = Path::new("/repo");
        assert!(diff_status(&BrokenGit, repo).is_err());
        assert!(diff_status(&BrokenAfter::new(1), repo).is_err());
        assert!(diff_status(&BrokenAfter::new(2), repo).is_err());
        assert!(diff_status(&BrokenAfter::new(3), repo).is_err());

        let branch_failure = FakeGit::new(vec![fail("not a repository")]);
        assert_eq!(diff_status(&branch_failure, repo).unwrap(), None);
        let remote_without_name = FakeGit::new(vec![ok("topic"), ok("")]);
        assert_eq!(diff_status(&remote_without_name, repo).unwrap(), None);
        let count_failure = FakeGit::new(vec![ok("topic"), ok("origin/main"), fail("bad range")]);
        assert_eq!(diff_status(&count_failure, repo).unwrap(), None);
        let missing_behind = FakeGit::new(vec![ok("topic"), ok("origin/main"), ok("x 1")]);
        assert_eq!(diff_status(&missing_behind, repo).unwrap(), None);
        let missing_ahead = FakeGit::new(vec![ok("topic"), ok("origin/main"), ok("1")]);
        assert_eq!(diff_status(&missing_ahead, repo).unwrap(), None);
        let stat_failure = FakeGit::new(vec![
            ok("topic"),
            ok("origin/main"),
            ok("1 2"),
            fail("bad diff"),
        ]);
        assert_eq!(diff_status(&stat_failure, repo).unwrap(), None);
    }
}

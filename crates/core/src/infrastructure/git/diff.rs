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
    let branch = runner.run(repo, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if !branch.success {
        return Ok(None);
    }
    let branch = branch.stdout.trim();
    if branch.is_empty() || branch == "HEAD" {
        return Ok(None);
    }

    let remote = runner.run(
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
    let counts = runner.run(repo, &["rev-list", "--left-right", "--count", &range])?;
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

    let stat = runner.run(repo, &["diff", "--numstat", "--merge-base", &base])?;
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
}

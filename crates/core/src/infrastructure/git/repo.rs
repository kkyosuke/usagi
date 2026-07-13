//! Repository-level git queries.

use std::path::Path;

use anyhow::Result;

use super::runner::GitRunner;

/// Whether `repo` is inside a git working tree.
///
/// # Errors
///
/// Returns an error only when the `git` process could not be spawned.
#[coverage(off)]
pub fn is_repository(runner: &dyn GitRunner, repo: &Path) -> Result<bool> {
    let output = runner.run(repo, &["rev-parse", "--is-inside-work-tree"])?;
    Ok(output.success && output.stdout.trim() == "true")
}

/// The short commit hash of `repo`'s current `HEAD`, or `None` when it cannot be
/// resolved (e.g. a repository with no commits yet).
///
/// # Errors
///
/// Returns an error only when the `git` process could not be spawned.
#[coverage(off)]
pub fn short_hash(runner: &dyn GitRunner, repo: &Path) -> Result<Option<String>> {
    let output = runner.run(repo, &["rev-parse", "--short", "HEAD"])?;
    Ok(output.success.then(|| output.stdout.trim().to_owned()))
}

#[cfg(test)]
mod tests {
    use super::{is_repository, short_hash};
    use crate::infrastructure::git::testkit::{FakeGit, fail, ok};
    use std::path::Path;

    #[test]
    fn is_repository_is_true_only_when_git_says_so() {
        let yes = FakeGit::new(vec![ok("true\n")]);
        assert!(is_repository(&yes, Path::new("/repo")).unwrap());
        // The command is scoped and correct.
        assert_eq!(
            yes.calls.borrow()[0],
            vec!["rev-parse", "--is-inside-work-tree"]
        );

        let no = FakeGit::new(vec![fail("fatal: not a git repository")]);
        assert!(!is_repository(&no, Path::new("/x")).unwrap());
        // A success with unexpected stdout is also not a repository.
        let weird = FakeGit::new(vec![ok("false")]);
        assert!(!is_repository(&weird, Path::new("/x")).unwrap());
    }

    #[test]
    fn short_hash_returns_the_trimmed_hash_or_none() {
        let some = FakeGit::new(vec![ok("abc1234\n")]);
        assert_eq!(
            short_hash(&some, Path::new("/repo")).unwrap().as_deref(),
            Some("abc1234")
        );
        let none = FakeGit::new(vec![fail("fatal: bad revision 'HEAD'")]);
        assert_eq!(short_hash(&none, Path::new("/repo")).unwrap(), None);
    }
}

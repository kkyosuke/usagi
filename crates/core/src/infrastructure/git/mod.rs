//! Git operations, behind an injected command seam.
//!
//! Every operation shells out to the system `git` binary (rather than linking a
//! git library) so the user's own git configuration is respected and the crate
//! stays dependency-light. The subprocess itself is the one piece of real IO, so
//! it is injected as a [`GitRunner`]: the operations here (`worktree`, `repo`)
//! take a `&dyn GitRunner` and stay pure and unit-testable, while the composition
//! root binds the real `git`-spawning implementation (mirroring the daemon's
//! `RecordFile` / `LivenessProbe` seams).

pub mod clone;
pub mod diff;
pub mod repo;
pub mod runner;
pub mod worktree;

pub use clone::clone;
pub use diff::{DiffStatus, diff_status};
pub use runner::{GitOutput, GitRunner};
pub use worktree::{WorktreeInfo, add_worktree, list_worktrees, remove_worktree};

#[cfg(test)]
pub(crate) mod testkit {
    //! A fake [`GitRunner`](super::GitRunner) for unit tests: it returns queued
    //! [`GitOutput`](super::GitOutput)s and records the argument lists it was
    //! called with, so a test can drive every branch (success, a specific stderr)
    //! without a real repository.

    use super::runner::{GitOutput, GitRunner};
    use std::cell::RefCell;
    use std::path::Path;

    /// A git output with a zero exit status and the given stdout.
    pub fn ok(stdout: &str) -> GitOutput {
        GitOutput {
            success: true,
            stdout: stdout.to_owned(),
            stderr: String::new(),
        }
    }

    /// A git output with a non-zero exit status and the given stderr.
    pub fn fail(stderr: &str) -> GitOutput {
        GitOutput {
            success: false,
            stdout: String::new(),
            stderr: stderr.to_owned(),
        }
    }

    /// A fake runner that pops one queued response per call and records each
    /// invocation's arguments.
    pub struct FakeGit {
        responses: RefCell<Vec<GitOutput>>,
        pub calls: RefCell<Vec<Vec<String>>>,
    }

    impl FakeGit {
        /// A fake that will return `responses` in order, one per `run` call.
        pub fn new(responses: Vec<GitOutput>) -> Self {
            Self {
                responses: RefCell::new(responses),
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl GitRunner for FakeGit {
        fn run(&self, _repo: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
            self.calls
                .borrow_mut()
                .push(args.iter().map(|s| (*s).to_owned()).collect());
            Ok(self.responses.borrow_mut().remove(0))
        }
    }
}

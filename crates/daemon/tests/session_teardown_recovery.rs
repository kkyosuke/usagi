//! A torn-down checkout can still have a live Git worktree registration.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use usagi_core::domain::id::{OperationId, SessionId};
use usagi_core::domain::session_lifecycle::SessionBranchScope;
use usagi_core::infrastructure::git::{GitRunner, list_worktrees};
use usagi_daemon::infrastructure::session_worktree::{SystemGit, SystemSessionWorktreeIo};
use usagi_daemon::usecase::session_runtime::{SessionWorktreeIo, WorktreeTeardown};
use usagi_daemon::usecase::session_teardown::{PendingTeardown, TeardownEffect};

struct Fixture {
    _temp: tempfile::TempDir,
    teardown: PendingTeardown,
    retained: BTreeSet<PathBuf>,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        fs::create_dir(root.join("data")).unwrap();
        let repository = root.join("repo");
        fs::create_dir(&repository).unwrap();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.name", "Recovery"],
            vec!["config", "user.email", "recovery@example.com"],
            vec!["commit", "-q", "--allow-empty", "-m", "root"],
        ] {
            git_ok(&repository, &args);
        }
        let container = repository.join(".usagi/sessions");
        let session = container.join("refactor");
        SystemSessionWorktreeIo
            .build_session_tree(&SystemGit, &repository, &session, "usagi/refactor", None)
            .unwrap();

        // A live sibling and an unrelated stale registration must both survive.
        let sibling = container.join("refactor-other");
        let stale = container.join("unrelated-stale");
        for (path, branch) in [
            (&sibling, "usagi/refactor-other"),
            (&stale, "usagi/unrelated-stale"),
        ] {
            SystemSessionWorktreeIo
                .build_session_tree(&SystemGit, &repository, path, branch, None)
                .unwrap();
        }
        fs::remove_dir_all(&stale).unwrap();
        let retained = BTreeSet::from([repository.clone(), sibling, stale]);
        Self {
            _temp: temp,
            retained,
            teardown: PendingTeardown {
                session_id: SessionId::new(),
                operation_id: OperationId::new(),
                name: "refactor".into(),
                repository_root: repository,
                data_home: root.join("data"),
                session_container: container,
                session_root: session,
                force: false,
                delete_branch: true,
                branch_scope: SessionBranchScope::IncludeTemporary,
                branch_name: None,
                force_delete_branch: false,
                merged_head_oid: None,
            },
        }
    }

    fn paths(&self) -> BTreeSet<PathBuf> {
        list_worktrees(&SystemGit, &self.teardown.repository_root)
            .unwrap()
            .into_iter()
            .map(|worktree| worktree.path)
            .collect()
    }

    fn branch_exists(&self) -> bool {
        SystemGit
            .run(
                &self.teardown.repository_root,
                &["rev-parse", "--verify", "refs/heads/usagi/refactor"],
            )
            .unwrap()
            .success
    }

    fn assert_recovered(&self) {
        let effect = WorktreeTeardown::new(SystemGit, SystemSessionWorktreeIo);
        effect.tear_down(&self.teardown).unwrap();
        assert!(!self.teardown.session_root.exists());
        assert!(!self.branch_exists());
        assert_eq!(self.paths(), self.retained);
        effect.tear_down(&self.teardown).unwrap();
        assert_eq!(self.paths(), self.retained);
    }
}

fn git_ok(repository: &Path, args: &[&str]) {
    let output = SystemGit.run(repository, args).unwrap();
    assert!(output.success, "git {args:?}: {}", output.stderr);
}

#[test]
fn retry_reclaims_the_registration_when_the_git_file_is_gone() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.teardown.session_root.join(".git")).unwrap();
    fs::write(
        fixture.teardown.session_root.join("remaining-build-output"),
        b"partial",
    )
    .unwrap();
    assert!(fixture.paths().contains(&fixture.teardown.session_root));
    fixture.assert_recovered();
}

#[test]
fn retry_reclaims_the_registration_when_the_checkout_is_gone() {
    let fixture = Fixture::new();
    fs::remove_dir_all(&fixture.teardown.session_root).unwrap();
    assert!(fixture.paths().contains(&fixture.teardown.session_root));
    fixture.assert_recovered();
}

#[test]
fn stale_owner_gitfile_is_terminal_for_temporary_branch_cleanup() {
    let fixture = Fixture::new();
    let root = &fixture.teardown.repository_root;
    // Simulate an owner checkout whose worktree metadata has already gone.
    fs::remove_dir_all(root.join(".git")).unwrap();
    fs::write(root.join(".git"), "gitdir: missing-owner-metadata\n").unwrap();
    WorktreeTeardown::new(SystemGit, SystemSessionWorktreeIo)
        .tear_down(&fixture.teardown)
        .unwrap();
    assert!(!fixture.teardown.session_root.exists());
}

#[test]
fn registration_recovery_still_protects_unmerged_commits() {
    let mut fixture = Fixture::new();
    git_ok(
        &fixture.teardown.session_root,
        &["commit", "-q", "--allow-empty", "-m", "unmerged work"],
    );
    fs::remove_dir_all(&fixture.teardown.session_root).unwrap();
    let effect = WorktreeTeardown::new(SystemGit, SystemSessionWorktreeIo);
    let error = effect.tear_down(&fixture.teardown).unwrap_err();
    assert!(error.contains("not fully merged"), "{error}");
    assert!(fixture.branch_exists());
    assert_eq!(fixture.paths(), fixture.retained);

    fixture.teardown.force = true;
    fixture.teardown.force_delete_branch = true;
    fixture.assert_recovered();
}

#[test]
fn registration_failure_keeps_the_branch_until_retry_can_remove_it() {
    let fixture = Fixture::new();
    let repository = &fixture.teardown.repository_root;
    let session = fixture.teardown.session_root.to_str().unwrap();
    git_ok(repository, &["worktree", "lock", "--", session]);
    fs::remove_dir_all(&fixture.teardown.session_root).unwrap();

    let error = WorktreeTeardown::new(SystemGit, SystemSessionWorktreeIo)
        .tear_down(&fixture.teardown)
        .unwrap_err();
    assert!(error.contains("git worktree remove failed"), "{error}");
    assert!(fixture.branch_exists());
    assert!(fixture.paths().contains(&fixture.teardown.session_root));

    git_ok(repository, &["worktree", "unlock", "--", session]);
    fixture.assert_recovered();
}

#[test]
fn teardown_removes_only_local_branches_in_the_session_temporary_namespace() {
    let fixture = Fixture::new();
    let repo = &fixture.teardown.repository_root;
    let session = &fixture.teardown.session_root;
    git_ok(session, &["switch", "-c", "usagi-tmp/refactor/checked-out"]);
    git_ok(session, &["branch", "usagi-tmp/refactor/unused/nested"]);
    git_ok(session, &["branch", "feature/keep"]);
    git_ok(session, &["branch", "usagi-tmp/refactor-other/topic"]);
    git_ok(session, &["tag", "usagi-tmp/refactor/checked-out"]);

    fixture.assert_recovered();
    let refs = SystemGit
        .run(repo, &["for-each-ref", "--format=%(refname)"])
        .unwrap()
        .stdout;
    assert!(!refs.contains("refs/heads/usagi-tmp/refactor/"));
    for retained in [
        "refs/heads/feature/keep",
        "refs/heads/usagi-tmp/refactor-other/topic",
        "refs/tags/usagi-tmp/refactor/checked-out",
    ] {
        assert!(refs.lines().any(|line| line == retained), "{refs}");
    }
}

#[test]
fn temporary_branch_retry_protects_unmerged_work_and_other_checkouts() {
    let mut fixture = Fixture::new();
    let branch = "usagi-tmp/refactor/wip";
    let repo = &fixture.teardown.repository_root;
    let session = &fixture.teardown.session_root;
    git_ok(session, &["switch", "-c", branch]);
    git_ok(session, &["commit", "--allow-empty", "-m", "wip"]);
    fixture.teardown.merged_head_oid = Some(
        SystemGit
            .run(session, &["rev-parse", "HEAD"])
            .unwrap()
            .stdout
            .trim()
            .to_owned(),
    );

    let effect = WorktreeTeardown::new(SystemGit, SystemSessionWorktreeIo);
    assert!(
        effect
            .tear_down(&fixture.teardown)
            .unwrap_err()
            .contains("not fully merged")
    );
    git_ok(repo, &["rev-parse", "--verify", branch]);
    let sibling = fixture.teardown.session_container.join("refactor-other");
    git_ok(&sibling, &["switch", branch]);
    fixture.teardown.force = true;
    fixture.teardown.force_delete_branch = true;
    assert!(effect.tear_down(&fixture.teardown).is_err());
    git_ok(repo, &["rev-parse", "--verify", branch]);
    git_ok(&sibling, &["switch", "usagi/refactor-other"]);
    fixture.assert_recovered();
    assert!(
        !SystemGit
            .run(
                repo,
                &["show-ref", "--verify", &format!("refs/heads/{branch}")]
            )
            .unwrap()
            .success
    );
}

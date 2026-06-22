use std::path::Path;

use super::command::git_capture;
use super::*;
use crate::domain::settings::BranchSource;

/// Run a git command in `dir`, asserting success.
fn run(dir: &Path, args: &[&str]) {
    assert!(
        test_command(dir).args(args).status().unwrap().success(),
        "git {args:?} failed"
    );
}

/// A repo on `main` with one commit and no remote.
fn init_repo(dir: &Path) {
    run(dir, &["init", "-q", "-b", "main"]);
    run(dir, &["config", "user.email", "t@e.com"]);
    run(dir, &["config", "user.name", "t"]);
    std::fs::write(dir.join("f"), "x").unwrap();
    run(dir, &["add", "."]);
    run(dir, &["commit", "-q", "-m", "init"]);
}

/// A repo with a remote, so `origin/*` refs and an upstream exist.
///
/// Built without `git clone` so the result does not depend on the host's
/// `init.defaultBranch` (which differs between developer machines and CI):
/// the work repo is created explicitly on `main`, then pushed with `-u` to
/// a bare remote to establish the upstream and `origin/main` ref.
fn repo_with_remote() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let bare = tmp.path().join("remote.git");
    let work = tmp.path().join("work");

    run(
        tmp.path(),
        &["init", "-q", "--bare", bare.to_str().unwrap()],
    );

    std::fs::create_dir_all(&work).unwrap();
    init_repo(&work);
    run(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
    // `-u` records origin/main as the upstream and creates the remote ref.
    run(&work, &["push", "-q", "-u", "origin", "main"]);
    // Point refs/remotes/origin/HEAD at origin/main explicitly.
    run(&work, &["remote", "set-head", "origin", "main"]);
    (tmp, work)
}

#[test]
fn lists_worktrees_with_primary_first() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());

    let worktrees = list_worktrees(dir.path()).unwrap();
    assert_eq!(worktrees.len(), 1);
    assert_eq!(worktrees[0].branch.as_deref(), Some("main"));
    assert!(!worktrees[0].head.is_empty());
    assert_eq!(primary_worktree(dir.path()).unwrap(), worktrees[0].path);
}

#[test]
fn errors_when_not_a_repository() {
    let dir = tempfile::tempdir().unwrap();
    assert!(list_worktrees(dir.path()).is_err());
    assert!(primary_worktree(dir.path()).is_err());
}

#[test]
fn lists_multiple_worktrees_including_a_detached_one() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let feature = dir.path().join("feature");
    let detached = dir.path().join("detached");
    run(
        dir.path(),
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            feature.to_str().unwrap(),
        ],
    );
    // A detached worktree emits a `detached` line (no `branch`), exercising
    // the parser's fall-through and yielding `branch: None`.
    run(
        dir.path(),
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            detached.to_str().unwrap(),
        ],
    );

    let worktrees = list_worktrees(dir.path()).unwrap();
    let branches: Vec<_> = worktrees
        .iter()
        .filter_map(|w| w.branch.as_deref())
        .collect();
    assert_eq!(worktrees.len(), 3);
    assert!(branches.contains(&"main"));
    assert!(branches.contains(&"feature"));
    // Exactly one worktree (the detached one) has no branch.
    assert_eq!(worktrees.iter().filter(|w| w.branch.is_none()).count(), 1);
}

#[test]
fn default_branch_prefers_remote_head() {
    let (_tmp, work) = repo_with_remote();
    assert_eq!(default_branch(&work), "main");
}

#[test]
fn default_branch_falls_back_without_remote() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    // No origin/HEAD: falls back to the checked-out branch.
    assert_eq!(default_branch(dir.path()), "main");
}

#[test]
fn default_branch_falls_back_to_main_when_detached() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    run(dir.path(), &["checkout", "-q", "--detach"]);
    // Detached HEAD and no remote: the hard-coded fallback applies.
    assert_eq!(default_branch(dir.path()), "main");
}

#[test]
fn ahead_behind_counts_against_local_and_remote() {
    let (_tmp, work) = repo_with_remote();
    // origin/main exists, so the remote ref is used as the target. main is
    // even with itself: nothing ahead, nothing behind.
    assert_eq!(ahead_behind(&work, "main", "main"), Some((0, 0)));

    let local = tempfile::tempdir().unwrap();
    init_repo(local.path());
    run(local.path(), &["branch", "feature"]);
    // No origin/main: the local branch is used. A freshly cut branch carries
    // no commits of its own and the default has not moved → (0, 0).
    assert_eq!(ahead_behind(local.path(), "feature", "main"), Some((0, 0)));

    // A commit on feature puts it one ahead of main, still zero behind.
    run(local.path(), &["checkout", "-q", "feature"]);
    std::fs::write(local.path().join("g"), "y").unwrap();
    run(local.path(), &["add", "."]);
    run(local.path(), &["commit", "-q", "-m", "ahead"]);
    assert_eq!(ahead_behind(local.path(), "feature", "main"), Some((1, 0)));

    // Advancing main past feature's base (a separate commit on main) makes
    // feature one behind as well as one ahead.
    run(local.path(), &["checkout", "-q", "main"]);
    std::fs::write(local.path().join("h"), "z").unwrap();
    run(local.path(), &["add", "."]);
    run(local.path(), &["commit", "-q", "-m", "main ahead"]);
    assert_eq!(ahead_behind(local.path(), "feature", "main"), Some((1, 1)));
}

#[test]
fn clone_copies_a_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    init_repo(&src);

    let dest = tmp.path().join("dest");
    clone(src.to_str().unwrap(), &dest, None).unwrap();

    assert!(dest.join(".git").is_dir());
    assert!(dest.join("f").is_file());
}

#[test]
fn clone_checks_out_the_requested_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    init_repo(&src);
    run(&src, &["branch", "feature"]);

    let dest = tmp.path().join("dest");
    clone(src.to_str().unwrap(), &dest, Some("feature")).unwrap();

    let head = git_capture(&dest, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap()
        .unwrap();
    assert_eq!(head, "feature");
}

#[test]
fn clone_fails_for_a_missing_source() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("nope");
    let dest = tmp.path().join("dest");

    let err = clone(missing.to_str().unwrap(), &dest, None).unwrap_err();
    assert!(err.to_string().contains("git clone failed"));
}

#[test]
fn short_hash_takes_first_seven_chars() {
    assert_eq!(short_hash("0123456789abcdef"), "0123456");
    assert_eq!(short_hash("abc"), "abc");
}

#[test]
fn add_worktree_creates_a_new_branch_checkout() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let dest = dir.path().join("wt");

    add_worktree(dir.path(), &dest, "feature", None).unwrap();

    // The new worktree exists and is checked out on the new branch.
    assert!(dest.join("f").is_file());
    let head = git_capture(&dest, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap()
        .unwrap();
    assert_eq!(head, "feature");
    // It is registered as a worktree of the repo (compare canonical paths,
    // since git resolves symlinks like macOS's /var -> /private/var).
    let canonical = dest.canonicalize().unwrap();
    let worktrees = list_worktrees(dir.path()).unwrap();
    assert!(worktrees.iter().any(|w| w
        .path
        .canonicalize()
        .map(|p| p == canonical)
        .unwrap_or(false)));
}

#[test]
fn worktree_status_reads_branch_head_upstream_and_dirtiness() {
    // A tracked branch on a clean tree: branch, full HEAD, upstream, not dirty.
    let (_tmp, work) = repo_with_remote();
    let status = worktree_status(&work).unwrap();
    assert_eq!(status.branch.as_deref(), Some("main"));
    assert_eq!(status.head.len(), 40);
    assert_eq!(status.upstream.as_deref(), Some("origin/main"));
    assert!(!status.dirty);

    // An untracked file marks the tree dirty.
    std::fs::write(work.join("new"), "y").unwrap();
    assert!(worktree_status(&work).unwrap().dirty);
}

#[test]
fn worktree_status_reports_no_branch_or_upstream_when_absent() {
    // A local repo with no remote: a branch but no upstream.
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let status = worktree_status(dir.path()).unwrap();
    assert_eq!(status.branch.as_deref(), Some("main"));
    assert_eq!(status.upstream, None);

    // A detached HEAD reports no branch.
    run(dir.path(), &["checkout", "-q", "--detach"]);
    assert_eq!(worktree_status(dir.path()).unwrap().branch, None);

    // A non-repo path yields nothing.
    let plain = tempfile::tempdir().unwrap();
    assert!(worktree_status(plain.path()).is_none());
}

#[test]
fn remove_worktree_deletes_a_clean_one_and_needs_force_when_dirty() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let clean = dir.path().join("clean");
    add_worktree(dir.path(), &clean, "clean", None).unwrap();
    remove_worktree(dir.path(), &clean, false).unwrap();
    assert!(!clean.exists());

    // A dirty worktree cannot be removed without force.
    let dirty = dir.path().join("dirty");
    add_worktree(dir.path(), &dirty, "dirty", None).unwrap();
    std::fs::write(dirty.join("scratch"), "z").unwrap();
    assert!(remove_worktree(dir.path(), &dirty, false).is_err());
    // ...but force discards it.
    remove_worktree(dir.path(), &dirty, true).unwrap();
    assert!(!dirty.exists());
}

#[test]
fn remove_worktree_is_a_noop_for_an_unregistered_path() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    // A path git has never registered as a worktree is already in the desired
    // end state, so removal succeeds rather than erroring — both for a path that
    // exists in the tree and one that does not.
    let stray = dir.path().join("stray");
    std::fs::create_dir_all(&stray).unwrap();
    remove_worktree(dir.path(), &stray, true).unwrap();
    remove_worktree(dir.path(), &dir.path().join("ghost"), true).unwrap();
}

#[test]
fn remove_worktree_forces_through_a_clean_worktree_containing_submodules() {
    let tmp = tempfile::tempdir().unwrap();

    // A standalone repo serves as the submodule source.
    let sub_src = tmp.path().join("sub-src");
    std::fs::create_dir_all(&sub_src).unwrap();
    init_repo(&sub_src);

    // Superproject embedding `sub-src` at `sub`. `-c protocol.file.allow` lifts
    // the git 2.38 block on local-path submodules for the child clone.
    let sup = tmp.path().join("super");
    std::fs::create_dir_all(&sup).unwrap();
    init_repo(&sup);
    run(
        &sup,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            sub_src.to_str().unwrap(),
            "sub",
        ],
    );
    run(&sup, &["commit", "-qm", "add submodule"]);

    // A worktree with the submodule checked out: git refuses to remove it
    // without `--force` purely because it contains a submodule, even though it
    // is clean. The non-forced call must still succeed by retrying forced.
    let wt = tmp.path().join("wt");
    add_worktree(&sup, &wt, "feat-x", None).unwrap();
    run(
        &wt,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
        ],
    );
    assert!(wt.join("sub").join("f").is_file());

    remove_worktree(&sup, &wt, false).unwrap();
    assert!(!wt.exists());
}

#[test]
fn prune_worktrees_clears_a_dangling_registration() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    // Register a worktree, then delete its directory out-of-band: git keeps a
    // dangling "prunable" registration that would block reusing the path.
    let wt = dir.path().join("wt");
    run(dir.path(), &["worktree", "add", "-q", wt.to_str().unwrap()]);
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    // Resolve the canonical path *before* deleting the directory — git reports
    // worktree paths canonicalized, and canonicalize fails once the dir is gone.
    let target = canon(&wt);
    std::fs::remove_dir_all(&wt).unwrap();
    let registered = |repo: &Path| {
        list_worktrees(repo)
            .unwrap()
            .iter()
            .any(|w| canon(&w.path) == target)
    };
    assert!(
        registered(dir.path()),
        "registration should linger before prune"
    );

    prune_worktrees(dir.path()).unwrap();

    // The stale registration is gone, so the path can be re-added.
    assert!(!registered(dir.path()));
    run(dir.path(), &["worktree", "add", "-q", wt.to_str().unwrap()]);
}

#[test]
fn prune_worktrees_errors_outside_a_repository() {
    let dir = tempfile::tempdir().unwrap();
    // No git repo here, so `git worktree prune` fails and the error is surfaced.
    let err = prune_worktrees(dir.path()).unwrap_err();
    assert!(err.to_string().contains("git worktree prune failed"));
}

#[test]
fn delete_branch_removes_a_branch_and_errors_when_missing() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    run(dir.path(), &["branch", "doomed"]);
    delete_branch(dir.path(), "doomed").unwrap();
    // Deleting it again fails (it is gone).
    assert!(delete_branch(dir.path(), "doomed").is_err());
}

#[test]
fn delete_branch_targets_a_leading_dash_name_as_a_branch() {
    // The session usecase rejects leading-`-` names, but `delete_branch` is
    // hardened with a `--` separator so a name like `-x` is treated as the
    // branch operand rather than a `git branch` option (e.g. `-x` / `-D`). The
    // ref is created via plumbing (`update-ref`) since `git branch` itself would
    // mis-parse the leading dash. Without the `--` the delete would fail with an
    // "unknown option" error instead of removing the branch.
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    run(dir.path(), &["update-ref", "refs/heads/-x", "HEAD"]);
    assert!(local_branches(dir.path()).iter().any(|b| b == "-x"));

    delete_branch(dir.path(), "-x").unwrap();
    assert!(!local_branches(dir.path()).iter().any(|b| b == "-x"));
}

#[test]
fn add_worktree_fails_for_an_existing_branch() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    run(dir.path(), &["branch", "feature"]);

    // `-b feature` cannot create a branch that already exists.
    let err = add_worktree(dir.path(), &dir.path().join("wt"), "feature", None).unwrap_err();
    assert!(err.to_string().contains("git worktree add failed"));
}

#[test]
fn add_worktree_branches_from_the_given_base() {
    // A repo with two commits: the session branch is cut from the *first*
    // commit (tagged `base`), proving the base ref is honoured rather than
    // the current HEAD.
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let base = git_capture(dir.path(), &["rev-parse", "HEAD"])
        .unwrap()
        .unwrap();
    run(dir.path(), &["branch", "base"]);
    std::fs::write(dir.path().join("f"), "second").unwrap();
    run(dir.path(), &["commit", "-aqm", "second"]);

    let dest = dir.path().join("wt");
    add_worktree(dir.path(), &dest, "feature", Some("base")).unwrap();

    let head = git_capture(&dest, &["rev-parse", "HEAD"]).unwrap().unwrap();
    assert_eq!(head, base);
}

#[test]
fn init_submodules_is_a_no_op_without_a_gitmodules() {
    // A repository with no submodules has no `.gitmodules`: the call succeeds
    // without spawning git.
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    init_submodules(dir.path()).unwrap();
}

#[test]
fn init_submodules_checks_out_an_uninitialized_submodule() {
    let tmp = tempfile::tempdir().unwrap();

    // A standalone repo (committing the file `f`) serves as the submodule source.
    let sub_src = tmp.path().join("sub-src");
    std::fs::create_dir_all(&sub_src).unwrap();
    init_repo(&sub_src);

    // The superproject embeds `sub-src` as a submodule at `sub`. Local-path
    // submodules are blocked by default since git 2.38; `-c` propagates the
    // allowance to the child `git clone` (via GIT_CONFIG_PARAMETERS), which a
    // repo-local config setting would not reach.
    let sup = tmp.path().join("super");
    std::fs::create_dir_all(&sup).unwrap();
    init_repo(&sup);
    run(
        &sup,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            sub_src.to_str().unwrap(),
            "sub",
        ],
    );
    run(&sup, &["commit", "-qm", "add submodule"]);

    // Deinit drops the working-tree checkout but keeps `.git/modules/sub`, so the
    // update re-checks-out from the existing module dir without cloning — the
    // same shape as a real https submodule that is already fetched, but offline.
    run(&sup, &["submodule", "deinit", "-f", "sub"]);
    assert!(sup.join(".gitmodules").is_file());
    assert!(!sup.join("sub").join("f").is_file());

    // init_submodules checks the submodule back out.
    init_submodules(&sup).unwrap();
    assert!(sup.join("sub").join("f").is_file());
}

#[test]
fn init_submodules_surfaces_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    run(dir.path(), &["config", "protocol.file.allow", "always"]);

    // A `.gitmodules` plus a gitlink pointing at a path that cannot be cloned
    // makes `git submodule update --init` fail.
    std::fs::write(
        dir.path().join(".gitmodules"),
        "[submodule \"sub\"]\n\tpath = sub\n\turl = ./missing\n",
    )
    .unwrap();
    let sha = git_capture(dir.path(), &["rev-parse", "HEAD"])
        .unwrap()
        .unwrap();
    run(
        dir.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{sha},sub"),
        ],
    );

    let err = init_submodules(dir.path()).unwrap_err();
    assert!(err.to_string().contains("git submodule update failed"));
}

#[test]
fn resolve_base_ref_prefers_remote_then_falls_back_to_local() {
    let (_tmp, work) = repo_with_remote();
    // With a remote, Remote resolves to origin/<default>...
    assert_eq!(
        resolve_base_ref(&work, BranchSource::Remote, None).as_deref(),
        Some("origin/main")
    );
    // ...while Local stays on the local branch.
    assert_eq!(
        resolve_base_ref(&work, BranchSource::Local, None).as_deref(),
        Some("main")
    );

    // Without a remote, Remote falls back to the local default branch.
    let local = tempfile::tempdir().unwrap();
    init_repo(local.path());
    assert_eq!(
        resolve_base_ref(local.path(), BranchSource::Remote, None).as_deref(),
        Some("main")
    );
    assert_eq!(
        resolve_base_ref(local.path(), BranchSource::Local, None).as_deref(),
        Some("main")
    );
}

#[test]
fn resolve_base_ref_honours_an_explicit_branch() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    run(dir.path(), &["branch", "develop"]);

    // A named branch overrides the detected default, in both forms.
    assert_eq!(
        resolve_base_ref(dir.path(), BranchSource::Local, Some("develop")).as_deref(),
        Some("develop")
    );
    // No origin/develop exists, so Remote falls back to the local branch.
    assert_eq!(
        resolve_base_ref(dir.path(), BranchSource::Remote, Some("develop")).as_deref(),
        Some("develop")
    );
    // A branch that does not resolve yields None (caller falls back to HEAD).
    assert_eq!(
        resolve_base_ref(dir.path(), BranchSource::Local, Some("ghost")),
        None
    );
}

#[test]
fn resolve_base_ref_is_none_without_the_default_branch() {
    // A fresh repo with no commits has no `main` ref, so there is nothing to
    // branch from and the caller should fall back to HEAD.
    let dir = tempfile::tempdir().unwrap();
    run(dir.path(), &["init", "-q", "-b", "main"]);
    assert_eq!(
        resolve_base_ref(dir.path(), BranchSource::Local, None),
        None
    );
    assert_eq!(
        resolve_base_ref(dir.path(), BranchSource::Remote, None),
        None
    );
}

#[test]
fn list_branches_returns_local_and_remote_names_deduped() {
    // A repo with a remote: local `main` plus a local `develop`, and the
    // remote-tracking `origin/main`. The duplicate `main` collapses and the
    // remote prefix is stripped, leaving a sorted, unique list.
    let (_tmp, work) = repo_with_remote();
    run(&work, &["branch", "develop"]);

    assert_eq!(list_branches(&work), vec!["develop", "main"]);

    // A branch that lives only on the remote still surfaces (prefix stripped).
    run(&work, &["branch", "feature/x"]);
    run(&work, &["push", "-q", "origin", "feature/x"]);
    run(&work, &["branch", "-D", "feature/x"]);
    assert_eq!(list_branches(&work), vec!["develop", "feature/x", "main"]);
}

#[test]
fn list_branches_is_empty_for_a_non_repo() {
    let plain = tempfile::tempdir().unwrap();
    assert!(list_branches(plain.path()).is_empty());
}

#[test]
fn local_branches_lists_only_local_refs() {
    let (_tmp, work) = repo_with_remote();
    run(&work, &["branch", "feature/x"]);
    // origin/main exists as a remote-tracking ref but must not appear: only
    // local branches constrain `worktree add -b`.
    let mut names = local_branches(&work);
    names.sort();
    assert_eq!(names, vec!["feature/x".to_string(), "main".to_string()]);

    // A non-repo has none.
    let plain = tempfile::tempdir().unwrap();
    assert!(local_branches(plain.path()).is_empty());
}

#[test]
fn branch_namespace_conflict_detects_nested_branches_only() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    // Branches nested under `test/` make a plain `test` branch impossible.
    run(dir.path(), &["branch", "test/tui-e2e"]);
    run(dir.path(), &["branch", "test/home-ui"]);

    // The clash is reported (first nested branch in ref order).
    assert_eq!(
        branch_namespace_conflict(dir.path(), "test").as_deref(),
        Some("test/home-ui")
    );

    // An exact branch is not a namespace clash (git reports that itself), and
    // an unrelated name is clear.
    assert_eq!(branch_namespace_conflict(dir.path(), "main"), None);
    assert_eq!(branch_namespace_conflict(dir.path(), "feature"), None);
    // A non-repo simply has no conflicts.
    let plain = tempfile::tempdir().unwrap();
    assert_eq!(branch_namespace_conflict(plain.path(), "test"), None);
}

#[test]
fn is_repository_detects_git_and_plain_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    assert!(is_repository(&repo));

    let plain = tmp.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    assert!(!is_repository(&plain));
}

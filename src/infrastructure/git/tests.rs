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
/// a bare remote (itself created on `main`) to establish the upstream and
/// `origin/main` ref. The bare repo needs `-b main` too: [`push_new_commit`]
/// clones it, and a clone checks out whatever branch the bare's HEAD names —
/// which without `-b main` follows the host's `init.defaultBranch` (`master`
/// on CI), leaving no local `main` to push back.
fn repo_with_remote() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let bare = tmp.path().join("remote.git");
    let work = tmp.path().join("work");

    // `-b main` pins the bare repo's HEAD to `main` so clones of it (see
    // [`push_new_commit`]) check out `main` regardless of `init.defaultBranch`.
    run(
        tmp.path(),
        &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
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
fn primary_of_an_empty_list_errors_instead_of_panicking() {
    // `git worktree list` always yields the current worktree on a real repo, but
    // a porcelain change or wrapper returning success with no `worktree` lines
    // would yield an empty list. That must surface as an error, not panic the
    // status-sync path.
    let err = super::worktree::primary_of(Vec::new(), Path::new("/repo")).unwrap_err();
    assert!(err.to_string().contains("/repo"));
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
fn diff_stat_counts_committed_and_tracked_changes_against_the_merge_base() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());

    // A tracked, three-line file on main so a later edit on the branch can
    // register removals against the merge-base.
    std::fs::write(dir.path().join("base.txt"), "L1\nL2\nL3\n").unwrap();
    run(dir.path(), &["add", "."]);
    run(dir.path(), &["commit", "-q", "-m", "base"]);

    // Cut feature off this point — the merge-base — then advance main past it
    // with a commit feature never sees, to prove the diff is taken against the
    // merge-base: main's later work must not count as the session's.
    run(dir.path(), &["branch", "feature"]);
    std::fs::write(dir.path().join("main-only.txt"), "x\ny\n").unwrap();
    run(dir.path(), &["add", "."]);
    run(dir.path(), &["commit", "-q", "-m", "main moves on"]);

    run(dir.path(), &["checkout", "-q", "feature"]);
    // Committed work on feature: a new three-line file (+3).
    std::fs::write(dir.path().join("a.txt"), "1\n2\n3\n").unwrap();
    run(dir.path(), &["add", "."]);
    run(dir.path(), &["commit", "-q", "-m", "feature work"]);
    // An uncommitted tracked edit: drop two of base.txt's three lines (-2).
    std::fs::write(dir.path().join("base.txt"), "L1\n").unwrap();
    // A staged new file (+1) and a staged binary file (ignored — numstat reports
    // `-`/`-`, exercising `sum_numstat`'s non-numeric branch).
    std::fs::write(dir.path().join("b.txt"), "9\n").unwrap();
    std::fs::write(dir.path().join("bin"), [0u8, 159, 146, 150]).unwrap();
    run(dir.path(), &["add", "b.txt", "bin"]);

    // +3 (a.txt) +1 (b.txt) added; -2 (base.txt) removed. The binary contributes
    // nothing, and main-only.txt lives past the merge-base, so it does not count.
    assert_eq!(diff_stat(dir.path(), "main"), Some((4, 2)));
}

#[test]
fn diff_stat_measures_against_the_remote_default_and_returns_none_for_an_unknown_base() {
    let (_tmp, work) = repo_with_remote();
    // origin/main exists, so the merge-base is resolved against the remote
    // default (the first branch of `diff_stat`'s `or_else`).
    run(&work, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(work.join("n.txt"), "a\nb\n").unwrap();
    run(&work, &["add", "."]);
    run(&work, &["commit", "-q", "-m", "two lines"]);
    assert_eq!(diff_stat(&work, "main"), Some((2, 0)));

    // A default that resolves to no ref at all has no merge-base → None.
    assert_eq!(diff_stat(&work, "does-not-exist"), None);
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
fn clone_refuses_a_remote_helper_transport() {
    // Defense in depth: even called directly (bypassing `RepoUrl`'s allow-list),
    // `clone` sets `GIT_ALLOW_PROTOCOL` so git refuses the `ext` remote helper
    // rather than running the embedded command. The marker file must not appear.
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join("pwned");
    let dest = tmp.path().join("dest");
    let payload = format!("ext::sh -c \"touch {}\"", marker.display());

    let err = clone(&payload, &dest, None).unwrap_err();
    assert!(err.to_string().contains("git clone failed"));
    assert!(!marker.exists(), "the ext helper command must not have run");
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
fn ensure_excluded_hides_an_untracked_path_from_status() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    // An untracked file makes the worktree dirty...
    std::fs::write(dir.path().join("artifact"), "x").unwrap();
    assert!(worktree_status(dir.path()).unwrap().dirty);

    // ...but once excluded it no longer counts as a change.
    ensure_excluded(dir.path(), "/artifact").unwrap();
    assert!(!worktree_status(dir.path()).unwrap().dirty);

    // The pattern landed in the local exclude file, not a tracked .gitignore.
    let exclude = std::fs::read_to_string(dir.path().join(".git/info/exclude")).unwrap();
    assert!(exclude.lines().any(|l| l == "/artifact"));
    assert!(!dir.path().join(".gitignore").exists());
}

#[test]
fn ensure_excluded_is_idempotent_and_preserves_existing_content() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let exclude = dir.path().join(".git/info/exclude");
    // Seed the exclude file with prior content that has no trailing newline, so
    // the appended pattern starts on its own line.
    std::fs::write(&exclude, "# existing\n*.tmp").unwrap();

    ensure_excluded(dir.path(), "/.claude/skills").unwrap();
    // A second call adds nothing further.
    ensure_excluded(dir.path(), "/.claude/skills").unwrap();

    let content = std::fs::read_to_string(&exclude).unwrap();
    assert!(content.contains("*.tmp"));
    assert_eq!(
        content.lines().filter(|l| *l == "/.claude/skills").count(),
        1
    );
}

#[test]
fn ensure_excluded_errors_outside_a_git_worktree() {
    let plain = tempfile::tempdir().unwrap();
    assert!(ensure_excluded(plain.path(), "/x").is_err());
}

#[test]
fn ensure_all_excluded_appends_every_missing_pattern_in_one_pass() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let exclude = dir.path().join(".git/info/exclude");
    std::fs::write(&exclude, "# existing\n/.claude/skills/a\n").unwrap();

    // Two of these are new; the middle one already present is left as-is.
    ensure_all_excluded(
        dir.path(),
        &[
            "/.claude/skills/a",
            "/.claude/skills/b",
            "/.claude/skills/c",
        ],
    )
    .unwrap();

    let content = std::fs::read_to_string(&exclude).unwrap();
    for pattern in [
        "/.claude/skills/a",
        "/.claude/skills/b",
        "/.claude/skills/c",
    ] {
        assert_eq!(content.lines().filter(|l| *l == pattern).count(), 1);
    }
    assert!(content.contains("# existing"));

    // A repeat run with the same set is a no-op (nothing further appended).
    let before = std::fs::read_to_string(&exclude).unwrap();
    ensure_all_excluded(dir.path(), &["/.claude/skills/a", "/.claude/skills/b"]).unwrap();
    assert_eq!(std::fs::read_to_string(&exclude).unwrap(), before);
}

#[test]
fn ensure_all_excluded_is_a_noop_for_no_patterns() {
    // No patterns means no work — and, notably, no git call, so this succeeds even
    // outside a git worktree.
    let plain = tempfile::tempdir().unwrap();
    assert!(ensure_all_excluded(plain.path(), &[]).is_ok());
}

#[test]
fn git_common_dir_resolves_inside_a_repo_and_is_none_outside() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let common = git_common_dir(dir.path()).expect("a repo has a common dir");
    assert!(common.ends_with(".git"));

    let plain = tempfile::tempdir().unwrap();
    assert!(git_common_dir(plain.path()).is_none());
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
fn remove_worktree_refuses_to_force_when_a_submodule_is_dirty_despite_ignore_config() {
    // The dangerous case the submodule force-escalation must not hit: a worktree
    // whose *submodule* holds uncommitted work, but whose `submodule.<name>.ignore`
    // config hides that from plain `git status`. A non-forced removal must refuse
    // rather than silently `--force` away the submodule changes.
    let tmp = tempfile::tempdir().unwrap();

    let sub_src = tmp.path().join("sub-src");
    std::fs::create_dir_all(&sub_src).unwrap();
    init_repo(&sub_src);

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

    // Dirty a tracked file *inside* the submodule, then tell git to ignore the
    // submodule's dirtiness in plain status — exactly the config that blinds the
    // upstream dirty gate.
    std::fs::write(wt.join("sub").join("f"), "uncommitted submodule work").unwrap();
    run(&wt, &["config", "submodule.sub.ignore", "all"]);
    // Plain status now reports the worktree clean, hiding the submodule change...
    assert!(!worktree_status(&wt).unwrap().dirty);

    // ...but the removal's config-independent re-check still sees it, so a
    // non-forced removal refuses instead of force-discarding the work.
    let err = remove_worktree(&sup, &wt, false).unwrap_err();
    assert!(err.to_string().contains("uncommitted changes"), "{err}");
    assert!(wt.exists());

    // An explicit force still removes it (the caller has opted into the loss).
    remove_worktree(&sup, &wt, true).unwrap();
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

// --- fetch / merge -------------------------------------------------------

/// The full HEAD commit at `dir`.
fn head_at(dir: &Path) -> String {
    git_capture(dir, &["rev-parse", "HEAD"])
        .unwrap()
        .unwrap_or_default()
}

/// Push a new commit to `bare`'s `main` from a throwaway clone, so that a repo
/// tracking `bare` becomes one commit behind `origin/main` after fetching.
/// `file`/`contents` let a caller stage a change that will conflict with local
/// work on the same path.
fn push_new_commit(bare: &Path, file: &str, contents: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let clone = tmp.path().join("pusher");
    run(
        tmp.path(),
        &[
            "clone",
            "-q",
            bare.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    run(&clone, &["config", "user.email", "t@e.com"]);
    run(&clone, &["config", "user.name", "t"]);
    std::fs::write(clone.join(file), contents).unwrap();
    run(&clone, &["add", "."]);
    run(&clone, &["commit", "-q", "-m", "remote work"]);
    run(&clone, &["push", "-q", "origin", "main"]);
    // Keep the temp clone alive until the push completes.
    drop(tmp);
}

#[test]
fn fetch_updates_remote_tracking_refs_and_errors_without_a_remote() {
    let (tmp, work) = repo_with_remote();
    // A new commit lands on the remote; fetching brings origin/main forward.
    push_new_commit(&tmp.path().join("remote.git"), "remote.txt", "hi");

    fetch(&work).unwrap();
    // origin/main now resolves to a commit local main does not have.
    let local = head_at(&work);
    let remote = git_capture(&work, &["rev-parse", "origin/main"])
        .unwrap()
        .unwrap();
    assert_ne!(local, remote);

    // A plain repo with no `origin` remote surfaces the failure.
    let plain = tempfile::tempdir().unwrap();
    init_repo(plain.path());
    let err = fetch(plain.path()).unwrap_err();
    assert!(err.to_string().contains("git fetch failed"), "{err}");
}

#[test]
fn merge_ff_only_fast_forwards_reports_up_to_date_and_refuses_to_diverge() {
    let (tmp, work) = repo_with_remote();
    push_new_commit(&tmp.path().join("remote.git"), "remote.txt", "hi");
    fetch(&work).unwrap();

    // Behind by one: a fast-forward advances HEAD to origin/main.
    let status = merge(&work, "origin/main", true).unwrap();
    assert_eq!(status, MergeStatus::Updated);
    assert_eq!(
        head_at(&work),
        git_capture(&work, &["rev-parse", "origin/main"])
            .unwrap()
            .unwrap()
    );

    // Now even with the remote: a second ff-only merge is a no-op.
    let status = merge(&work, "origin/main", true).unwrap();
    assert_eq!(status, MergeStatus::AlreadyUpToDate);

    // Local commits the remote lacks make a fast-forward impossible: the merge
    // refuses rather than creating a merge commit, and HEAD is untouched.
    push_new_commit(&tmp.path().join("remote.git"), "remote.txt", "moved on");
    fetch(&work).unwrap();
    std::fs::write(work.join("local.txt"), "local work").unwrap();
    run(&work, &["add", "."]);
    run(&work, &["commit", "-q", "-m", "local work"]);
    let head_before = head_at(&work);
    let status = merge(&work, "origin/main", true).unwrap();
    assert_eq!(status, MergeStatus::NotFastForward);
    assert_eq!(head_at(&work), head_before);
}

#[test]
fn merge_creates_a_merge_commit_and_aborts_a_conflict() {
    let (tmp, work) = repo_with_remote();
    // The remote advances on `f` (the file init_repo committed).
    push_new_commit(&tmp.path().join("remote.git"), "f", "remote change\n");
    fetch(&work).unwrap();

    // Local work on a *different* file merges cleanly into a merge commit.
    std::fs::write(work.join("local.txt"), "local\n").unwrap();
    run(&work, &["add", "."]);
    run(&work, &["commit", "-q", "-m", "local work"]);
    let status = merge(&work, "origin/main", false).unwrap();
    assert_eq!(status, MergeStatus::Updated);
    // Both changes are present after the merge.
    assert_eq!(
        std::fs::read_to_string(work.join("f")).unwrap(),
        "remote change\n"
    );
    assert!(work.join("local.txt").exists());

    // The remote changes `f` again; local edits the same line differently and
    // commits, so the next merge conflicts — it must abort and restore HEAD.
    push_new_commit(&tmp.path().join("remote.git"), "f", "remote v2\n");
    fetch(&work).unwrap();
    std::fs::write(work.join("f"), "local v2\n").unwrap();
    run(&work, &["add", "."]);
    run(&work, &["commit", "-q", "-m", "local edit of f"]);
    let head_before = head_at(&work);
    let status = merge(&work, "origin/main", false).unwrap();
    assert_eq!(status, MergeStatus::Conflict);
    // Aborted: HEAD is restored and no merge is in progress.
    assert_eq!(head_at(&work), head_before);
    assert_eq!(
        std::fs::read_to_string(work.join("f")).unwrap(),
        "local v2\n"
    );
    assert!(
        git_capture(&work, &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"])
            .unwrap()
            .is_none()
    );

    drop(tmp);
}

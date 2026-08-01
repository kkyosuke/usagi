//! The daemon's Git seam ignores an inherited repository environment.
//!
//! Git resolves its repository, index, object database and configuration from
//! the `GIT_*` namespace ahead of the `-C <repo>` the daemon passes, so a
//! poisoned environment would aim session create / remove at a different
//! repository. This drives the real `git` binary against a two-repository
//! fixture: the operations name `target`, the environment names `decoy`, and
//! only `target` may change.
//!
//! It is deliberately **one** test. It mutates this process' environment, which
//! is process-global, so a second test in this binary could observe a
//! half-installed hostile environment. Every Git effect of a session (create, the
//! nested worktrees of a mirrored tree, remove) is driven inside it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use usagi_core::infrastructure::git::{GitOutput, GitRunner, list_worktrees};
use usagi_daemon::infrastructure::session_worktree::{SystemGit, SystemSessionWorktreeIo};
use usagi_daemon::usecase::session_runtime::SessionWorktreeIo;

const BRANCH: &str = "usagi/confinement";

#[test]
fn a_hostile_git_environment_cannot_redirect_a_session_worktree_effect() {
    let home = tempfile::tempdir().expect("temp dir");
    let root = fs::canonicalize(home.path()).expect("canonical fixture root");

    // The repository the daemon names, and the one the environment names.
    let target = init_repository(&root.join("target"));
    let decoy = init_repository(&root.join("decoy"));
    // A workspace root that is not a repository itself: `build_session_tree`
    // mirrors it and adds a worktree for each repository it finds inside.
    let mirror_root = root.join("mirror");
    let nested = init_repository(&mirror_root.join("nested"));
    fs::write(mirror_root.join("note.txt"), b"kept\n").expect("plain file");

    let hostile_index = root.join("hostile-index");
    let hostile_objects = root.join("hostile-objects");
    let hostile_hooks = root.join("hostile-hooks");
    install_hostile_environment(&decoy, &hostile_index, &hostile_objects, &hostile_hooks);

    let git = SystemGit;
    let io = SystemSessionWorktreeIo;

    // The scope of a command is the path it names, not the inherited GIT_DIR.
    let toplevel = ok(&git, &target, &["rev-parse", "--show-toplevel"]);
    assert_eq!(Path::new(toplevel.stdout.trim()), target);

    // The injected config is gone: with GIT_CONFIG_COUNT honoured this lookup
    // would succeed and hand git a hooks directory the caller chose.
    let hooks = git
        .run(&target, &["config", "--get", "core.hooksPath"])
        .expect("git config");
    assert!(
        !hooks.success && hooks.stdout.trim().is_empty(),
        "config injection survived: {hooks:?}"
    );

    // create: a repository workspace root becomes a worktree of that repository.
    let session_root = root.join("sessions/direct");
    io.build_session_tree(&git, &target, &session_root, BRANCH)
        .expect("build session tree");
    assert_eq!(
        worktree_paths(&git, &target),
        vec![target.clone(), session_root.clone()]
    );

    // create, nested: each repository inside a mirrored tree gets its own
    // worktree at the same relative path, and plain files are copied.
    let mirror_session = root.join("sessions/mirror");
    io.build_session_tree(&git, &mirror_root, &mirror_session, BRANCH)
        .expect("build mirrored session tree");
    assert_eq!(
        worktree_paths(&git, &nested),
        vec![nested.clone(), mirror_session.join("nested")]
    );
    assert_eq!(
        fs::read(mirror_session.join("note.txt")).expect("copied file"),
        b"kept\n"
    );

    // remove: the worktrees of the session tree are removed from the repository
    // the tree belongs to.
    io.remove_session_tree(&git, &session_root, true)
        .expect("remove session tree");
    io.remove_session_tree(&git, &mirror_session, true)
        .expect("remove mirrored session tree");
    assert_eq!(worktree_paths(&git, &target), vec![target.clone()]);
    assert_eq!(worktree_paths(&git, &nested), vec![nested.clone()]);
    assert!(!session_root.exists() && !mirror_session.exists());

    // The repository the environment named was never touched by any of it: no
    // branch, no worktree, and no index or object database at the paths the
    // environment pointed at.
    assert_eq!(worktree_paths(&git, &decoy), vec![decoy.clone()]);
    assert_eq!(
        ok(&git, &decoy, &["branch", "--format=%(refname:short)"])
            .stdout
            .trim(),
        "main"
    );
    assert!(
        !hostile_index.exists(),
        "an index was written to GIT_INDEX_FILE"
    );
    assert!(
        !hostile_objects.exists(),
        "objects were written to GIT_OBJECT_DIRECTORY"
    );

    remove_hostile_environment();
}

/// Point every repository-selecting variable git reads at `decoy`, and inject a
/// config value through the `GIT_CONFIG_COUNT` protocol.
fn install_hostile_environment(decoy: &Path, index: &Path, objects: &Path, hooks: &Path) {
    for (name, value) in hostile_bindings(decoy, index, objects, hooks) {
        // SAFETY: this test is the only one in this binary and spawns no
        // threads, so nothing can read the environment concurrently.
        unsafe { std::env::set_var(name, value) };
    }
}

fn remove_hostile_environment() {
    for (name, _) in hostile_bindings(Path::new(""), Path::new(""), Path::new(""), Path::new("")) {
        // SAFETY: as in `install_hostile_environment`.
        unsafe { std::env::remove_var(name) };
    }
}

fn hostile_bindings(
    decoy: &Path,
    index: &Path,
    objects: &Path,
    hooks: &Path,
) -> Vec<(&'static str, PathBuf)> {
    vec![
        ("GIT_DIR", decoy.join(".git")),
        ("GIT_WORK_TREE", decoy.to_path_buf()),
        ("GIT_COMMON_DIR", decoy.join(".git")),
        ("GIT_INDEX_FILE", index.to_path_buf()),
        ("GIT_OBJECT_DIRECTORY", objects.to_path_buf()),
        ("GIT_NAMESPACE", PathBuf::from("hostile")),
        ("GIT_CEILING_DIRECTORIES", decoy.to_path_buf()),
        ("GIT_CONFIG_COUNT", PathBuf::from("1")),
        ("GIT_CONFIG_KEY_0", PathBuf::from("core.hooksPath")),
        ("GIT_CONFIG_VALUE_0", hooks.to_path_buf()),
    ]
}

/// A repository with one commit on `main`, built before the hostile environment
/// is installed so the fixture itself is unaffected by it.
fn init_repository(path: &Path) -> PathBuf {
    fs::create_dir_all(path).expect("repository directory");
    let path = fs::canonicalize(path).expect("canonical repository path");
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["config", "user.email", "confinement@example.com"],
        vec!["config", "user.name", "Confinement"],
        vec!["commit", "-q", "--allow-empty", "-m", "root"],
    ] {
        let output = Command::new("git")
            .arg("-C")
            .arg(&path)
            .args(&args)
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    path
}

fn ok(git: &SystemGit, repo: &Path, args: &[&str]) -> GitOutput {
    let output = git.run(repo, args).expect("spawn git");
    assert!(output.success, "git {args:?} failed: {}", output.stderr);
    output
}

/// The worktree paths git reports for `repo`, canonicalised so the fixture's own
/// symlinked temporary directory cannot make an equal path compare unequal.
fn worktree_paths(git: &SystemGit, repo: &Path) -> Vec<PathBuf> {
    list_worktrees(git, repo)
        .expect("list worktrees")
        .into_iter()
        .map(|info| fs::canonicalize(&info.path).unwrap_or(info.path))
        .collect()
}

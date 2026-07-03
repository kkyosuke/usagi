//! Tests for the `diff` command: the `selected_diff` gatherer (the impure git
//! half) and the `:diff` palette wiring that opens the right-pane diff view.

use super::*;

/// Run a git command in `dir`, asserting it succeeds.
fn git(dir: &Path, args: &[&str]) {
    assert!(
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("LC_ALL", "C")
            .status()
            .unwrap()
            .success(),
        "git {args:?} failed"
    );
}

/// A repo whose HEAD is on `feature` (a file added past `main`) with `origin/main`
/// established, so `default_branch` resolves to `main` and the diff against it is
/// non-empty — the shape of a real usagi session worktree. Built without `git
/// clone` so the result does not depend on the host's `init.defaultBranch`.
fn repo_with_feature_diff(root: &Path) -> std::path::PathBuf {
    let bare = root.join("remote.git");
    let work = root.join("work");
    git(
        root,
        &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
    );

    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    git(&work, &["config", "user.email", "t@e.com"]);
    git(&work, &["config", "user.name", "t"]);
    std::fs::write(work.join("base"), "x\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&work, &["push", "-q", "-u", "origin", "main"]);
    git(&work, &["remote", "set-head", "origin", "main"]);

    // Cut feature off main and add a file, so the diff against origin/main shows it.
    git(&work, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(work.join("added.txt"), "1\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "feature work"]);
    work
}

/// A home state whose single session's worktree is on `feature` at `path`.
fn state_with_worktree_at(path: &Path) -> HomeState {
    HomeState::new(
        "usagi",
        vec![worktree(Some("feature"), path.to_str().unwrap())],
        None,
    )
}

#[test]
fn selected_diff_gathers_the_patch_for_the_highlighted_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let work = repo_with_feature_diff(dir.path());

    let mut state = state_with_worktree_at(&work);
    state.switch_select(1); // row 0 is the root; row 1 is the feature worktree

    let (title, patch) = selected_diff(&state).expect("a diff is gathered");
    assert_eq!(title, "feature → main");
    assert!(patch.contains("added.txt"), "patch: {patch}");
}

#[test]
fn selected_diff_fails_without_a_highlighted_session() {
    // The cursor on the root row (no session) has no worktree to diff.
    let state = sample_state(); // starts on the root row
    let err = selected_diff(&state).unwrap_err().to_string();
    assert!(err.contains("highlight a session"), "err: {err}");
}

#[test]
fn selected_diff_fails_when_the_base_ref_is_unresolvable() {
    // A worktree path that is not a git repository: the base cannot be resolved,
    // so no diff view opens.
    let dir = tempfile::tempdir().unwrap();
    let mut state = state_with_worktree_at(dir.path());
    state.switch_select(1);
    let err = selected_diff(&state).unwrap_err().to_string();
    assert!(err.contains("base branch"), "err: {err}");
}

#[test]
fn diff_command_opens_the_diff_view_for_a_real_repo() {
    // Driving `:diff` end-to-end over a real repo opens the scrollable diff view;
    // the arrows scroll it and Esc dismisses it back to 切替 (Ctrl-C quits).
    let dir = tempfile::tempdir().unwrap();
    let work = repo_with_feature_diff(dir.path());
    let mut state = state_with_worktree_at(&work);
    state.switch_select(1);

    let mut keys = cmd("diff");
    keys.push(Ok(Key::Enter)); // run `diff` -> gathers the patch, opens the pane
    keys.push(Ok(Key::ArrowDown)); // scroll down a line
    keys.push(Ok(Key::Escape)); // dismiss -> Switch
    keys.push(Ok(Key::Escape)); // inert at the base Switch; fallback Ctrl-C quits
    assert!(matches!(run(keys, state).unwrap(), Outcome::Quit));
}

#[test]
fn diff_command_logs_a_failure_when_no_session_is_highlighted() {
    // On the root row, `:diff` opens nothing and logs the error; the screen keeps
    // running and quits on the trailing Ctrl-C.
    let mut keys = cmd("diff");
    keys.push(Ok(Key::Enter)); // run `diff` -> no selection, logs and opens nothing
    keys.push(Ok(Key::Escape)); // Esc inert at the base Switch (no preview captured it)
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

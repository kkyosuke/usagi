//! Tests for the `diff` command: the `selected_diff` gatherer (the impure git
//! half) and the 集中 (Closeup) wiring that opens the right-pane diff view (`diff`
//! is a session-scoped command, run from the Closeup menu / prompt).

use super::*;

/// Run a git command in `dir`, asserting it succeeds.
///
/// Strips git's repo-scoping env vars (the same set as
/// `infrastructure::git::command::REPO_SCOPING_ENV`): when `cargo test` runs
/// under a git hook (lefthook pre-push), git exports `GIT_DIR` etc., which
/// would override `-C` and point these commands at the hook's repository
/// instead of the temp repo under construction.
fn git(dir: &Path, args: &[&str]) {
    let mut command = std::process::Command::new("git");
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_COMMON_DIR",
        "GIT_PREFIX",
        "GIT_NAMESPACE",
    ] {
        command.env_remove(var);
    }
    assert!(
        command
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
    state.overview_select(1); // row 0 is the root; row 1 is the feature worktree

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
    state.overview_select(1);
    let err = selected_diff(&state).unwrap_err().to_string();
    assert!(err.contains("base branch"), "err: {err}");
}

#[test]
fn diff_command_opens_the_diff_view_from_the_closeup_menu_for_a_real_repo() {
    // Driving the 集中 (Closeup) `diff` command end-to-end over a real repo opens the
    // GitHub-style diff view: focus the session, move the menu cursor onto `diff`
    // (agent → close → diff), and run it. Then exercise the focus-aware keys — the
    // explorer navigates, `Enter`/`→`/`l` open a file into the diff pane, the diff
    // pane scrolls, and `q`/`Esc` dismiss back to 集中, then out to 選択.
    let dir = tempfile::tempdir().unwrap();
    let work = repo_with_feature_diff(dir.path());
    let state = state_with_worktree_at(&work);

    let mut keys = vec![
        Ok(Key::ArrowDown), // cursor root -> the feature session
        Ok(Key::Enter),     // focus it (idle -> Closeup menu, "agent" highlighted)
        Ok(Key::ArrowDown), // agent -> close
        Ok(Key::ArrowDown), // close -> diff
        Ok(Key::Enter),     // run `diff` -> gathers the patch, opens the pane
    ];
    // Explorer (Tree focus): move the cursor and try to collapse (no-op on a file).
    keys.push(Ok(Key::ArrowDown)); // tree move down
    keys.push(Ok(Key::Char('j'))); // tree move down (vi)
    keys.push(Ok(Key::ArrowUp)); // tree move up
    keys.push(Ok(Key::Char('k'))); // tree move up (vi)
    keys.push(Ok(Key::ArrowLeft)); // collapse (no-op on a file leaf)
    keys.push(Ok(Key::Char('h'))); // collapse (no-op, vi)
    keys.push(Ok(Key::ArrowRight)); // open the file -> focus the diff pane
    keys.push(Ok(Key::ArrowLeft)); // diff pane: back to the explorer
    keys.push(Ok(Key::Char('l'))); // open the file (vi) -> focus the diff pane
    keys.push(Ok(Key::Char('h'))); // diff pane: back to the explorer (vi)
    keys.push(Ok(Key::Enter)); // open the file -> focus the diff pane
                               // Diff pane focus: scroll and toggle the layout.
    keys.push(Ok(Key::ArrowDown)); // scroll down a line
    keys.push(Ok(Key::Char('j'))); // scroll down (vi)
    keys.push(Ok(Key::ArrowUp)); // scroll up a line
    keys.push(Ok(Key::Char('k'))); // scroll up (vi)
    keys.push(Ok(Key::PageDown)); // page down
    keys.push(Ok(Key::Char(' '))); // Space also pages forward
    keys.push(Ok(Key::PageUp)); // page up
    keys.push(Ok(Key::Char('s'))); // toggle to the split layout
    keys.push(Ok(Key::Char('v'))); // stack the explorer above the diff
    keys.push(Ok(Key::Char('v'))); // back to side by side
    keys.push(Ok(Key::Char('x'))); // ignored while the diff pane is focused
    keys.push(Ok(Key::Tab)); // toggle focus back to the explorer
    keys.push(Ok(Key::Char('z'))); // ignored while the explorer is focused
    keys.push(Ok(Key::Char('q'))); // dismiss -> back to Closeup
    keys.push(Ok(Key::Escape)); // Closeup -> base Overview; fallback Ctrl-C quits
    assert!(matches!(run(keys, state).unwrap(), Outcome::Quit));
}

#[test]
fn diff_command_opens_the_diff_view_from_the_closeup_prompt() {
    // Typed into the 集中 prompt, `diff` opens the same view for the focused
    // session; Esc dismisses it back to 集中, then out to 選択.
    let dir = tempfile::tempdir().unwrap();
    let work = repo_with_feature_diff(dir.path());
    let mut state = state_with_worktree_at(&work);
    state.set_session_action_ui(SessionActionUi::Prompt);

    let mut keys = vec![
        Ok(Key::ArrowDown), // cursor root -> the feature session
        Ok(Key::Enter),     // focus it (idle -> Closeup prompt)
    ];
    keys.extend(typed("diff"));
    keys.push(Ok(Key::Enter)); // run `diff` -> gathers the patch, opens the pane
    keys.push(Ok(Key::Escape)); // dismiss -> back to Closeup
    keys.push(Ok(Key::Escape)); // Closeup -> base Overview; fallback Ctrl-C quits
    assert!(matches!(run(keys, state).unwrap(), Outcome::Quit));
}

#[test]
fn diff_tab_is_left_and_reopened_with_the_tab_switch_keys() {
    // The diff is a session tab: `Ctrl-N` / `Ctrl-P` (the tab-switch keys) leave it
    // back to 集中, and re-running `diff` (the menu cursor stays on it) reopens it.
    let dir = tempfile::tempdir().unwrap();
    let work = repo_with_feature_diff(dir.path());
    let state = state_with_worktree_at(&work);

    let keys = vec![
        Ok(Key::ArrowDown),    // cursor root -> the feature session
        Ok(Key::Enter),        // focus it (idle -> Closeup menu, "agent")
        Ok(Key::ArrowDown),    // agent -> close
        Ok(Key::ArrowDown),    // close -> diff
        Ok(Key::Enter),        // open the diff tab
        Ok(Key::Char(CTRL_N)), // tab-next leaves the diff tab -> back to 集中
        Ok(Key::Enter),        // the cursor stayed on `diff`; reopen it
        Ok(Key::Char(CTRL_P)), // tab-prev leaves the diff tab -> back to 集中
        Ok(Key::Escape),       // 集中 -> base Overview; fallback Ctrl-C quits
    ];
    assert!(matches!(run(keys, state).unwrap(), Outcome::Quit));
}

#[test]
fn diff_command_logs_a_failure_when_no_session_is_focused() {
    // Focused on the root row (no session) the `diff` command — typed into the 集中
    // prompt, where it is offered even on root — gathers nothing, so it logs the
    // error and opens no view. The screen keeps running and quits on the trailing
    // Ctrl-C.
    let mut state = sample_state(); // starts on the root row
    state.set_session_action_ui(SessionActionUi::Prompt);
    let mut keys = vec![Ok(Key::Enter)]; // focus the root row (idle -> Closeup prompt)
    keys.extend(typed("diff"));
    keys.push(Ok(Key::Enter)); // run `diff` -> no session, logs and opens nothing
    keys.push(Ok(Key::Escape)); // Closeup -> base Overview (no diff view captured it)
    assert!(matches!(run(keys, state).unwrap(), Outcome::Quit));
}

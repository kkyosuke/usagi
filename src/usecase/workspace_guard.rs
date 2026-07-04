//! Whether an agent's tool call is allowed given where the agent is running.
//!
//! A usagi session worktree lives *inside* the main repository
//! (`<repo>/.usagi/sessions/<name>/`), so the repository root and its other
//! worktrees sit just above it on disk. An agent that edits `<repo>/src/...` or
//! `cd`s up into the repo is touching the wrong tree — a recurring foot-gun, see
//! [`crate::presentation::cli::guard_workspace`] for how this is wired into
//! Claude Code as a `PreToolUse` guard.
//!
//! This module is the pure decision behind that guard, in two modes keyed off
//! the agent's `cwd`:
//!
//! - **Session mode** ([`escapes_worktree`]): the agent runs inside a session
//!   worktree, and may edit anything inside it. Given the worktree and the path
//!   a tool wants to touch, does that path escape the worktree? The path is
//!   resolved *lexically* (folding `.` / `..` without touching the filesystem)
//!   so it works for files that do not exist yet (a fresh `Write` target) and
//!   stays deterministic under test.
//! - **Root mode** ([`is_write_tool`] / [`command_mutates_repo`]): the agent is
//!   the coordinator running at the workspace root (cwd is *not* under
//!   `.usagi/sessions/`, see [`is_session_worktree`]). It must not mutate the
//!   repository at all, so *every* file-writing tool is denied regardless of
//!   path, and `Bash` calls are denied when they invoke a repository-mutating
//!   git subcommand (read-only git like `status` / `log` / `diff` still runs).

use std::path::{Component, Path, PathBuf};

/// Whether `cwd` is inside a usagi session worktree
/// (`<repo>/.usagi/sessions/<name>/…`), rather than the workspace root the
/// coordinator runs at. This mirrors the pre-commit hook's exemption, which
/// keys off the same `.usagi/sessions/` path segment (see
/// [06-conventions.md](../../document/06-conventions.md) / `lefthook.yml`): a
/// path is a session worktree when consecutive `.usagi` → `sessions`
/// components appear in it. The guard uses this to pick session mode vs the
/// stricter root mode.
pub fn is_session_worktree(cwd: &Path) -> bool {
    let names: Vec<&std::ffi::OsStr> = cwd
        .components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();
    names
        .windows(2)
        .any(|pair| pair[0] == ".usagi" && pair[1] == "sessions")
}

/// Tools that write to the filesystem, denied wholesale in root mode. Matched
/// case-sensitively against the hook payload's `tool_name`. `Bash` is not here:
/// it is inspected per-command by [`command_mutates_repo`] so read-only shell
/// (and read-only git) still runs.
const WRITE_TOOLS: &[&str] = &["Write", "Edit", "MultiEdit", "NotebookEdit"];

/// Whether `tool_name` is a file-writing tool that root mode denies outright.
pub fn is_write_tool(tool_name: &str) -> bool {
    WRITE_TOOLS.contains(&tool_name)
}

/// Git subcommands that only read the repository, so they stay allowed in root
/// mode. Everything else that reaches `git` is treated as potentially mutating
/// and denied — an allow-list is the fail-safe choice here (an unknown or
/// ambiguous git subcommand like `config` / `branch` / `remote` is blocked
/// rather than let through), and the coordinator has no need to run them
/// against the main repository anyway.
const READ_ONLY_GIT_SUBCOMMANDS: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "blame",
    "reflog",
    "shortlog",
    "describe",
    "rev-parse",
    "rev-list",
    "ls-files",
    "ls-tree",
    "ls-remote",
    "cat-file",
    "show-ref",
    "name-rev",
    "merge-base",
    "whatchanged",
    "grep",
    "cherry",
    "diff-tree",
    "diff-index",
    "diff-files",
    "for-each-ref",
    "count-objects",
    "verify-commit",
    "verify-tag",
    "var",
    "help",
    "version",
];

/// Command wrappers that may precede `git` in a shell command; skipped when
/// locating the `git` invocation so `sudo git commit` / `env git push` are
/// still caught.
const COMMAND_WRAPPERS: &[&str] = &["sudo", "command", "env", "nohup", "time"];

/// Pre-subcommand git global options that consume the following token as their
/// value (e.g. `git -C /path commit`), so the value is not mistaken for the
/// subcommand.
const GIT_OPTS_WITH_VALUE: &[&str] = &[
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--exec-path",
    "--config-env",
];

/// Whether a `Bash` `command` would run a repository-mutating git subcommand.
///
/// The command is split on the usual shell separators (`&&`, `||`, `|`, `;`,
/// `&`, newline) into simple commands, and each is inspected for a `git`
/// invocation whose subcommand is *not* in [`READ_ONLY_GIT_SUBCOMMANDS`]. Any
/// such invocation makes the whole command mutating — so `git status && git
/// commit …` is denied on the `commit`. This is a lexical heuristic (it does
/// not evaluate command substitution or quoting), which is the robust,
/// fail-safe direction: it errs toward denying rather than missing a mutation.
pub fn command_mutates_repo(command: &str) -> bool {
    split_simple_commands(command)
        .into_iter()
        .filter_map(git_subcommand)
        .any(|sub| !READ_ONLY_GIT_SUBCOMMANDS.contains(&sub))
}

/// Split a shell command into its simple commands by folding every shell
/// separator (`&&`, `||`, `|`, `;`, `&`, CR/LF) to a newline and splitting on
/// it. Longer operators are folded before their single-character prefixes so
/// `&&` / `||` do not leave a stray `&` / `|`.
fn split_simple_commands(command: &str) -> Vec<&str> {
    command
        .split(['\n', '\r', ';', '&', '|'])
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect()
}

/// The git subcommand a single simple command would run, or `None` when it does
/// not invoke `git`. Leading `VAR=value` assignments and command wrappers
/// (`sudo`, `env`, …) are skipped to reach `git`; then git's pre-subcommand
/// options are skipped — including the ones that consume a following value — so
/// the first non-option token is the subcommand.
fn git_subcommand(segment: &str) -> Option<&str> {
    let mut tokens = segment.split_whitespace().peekable();
    while let Some(&token) = tokens.peek() {
        if is_env_assignment(token) || COMMAND_WRAPPERS.contains(&token) {
            tokens.next();
        } else {
            break;
        }
    }
    if tokens.next()? != "git" {
        return None;
    }
    while let Some(token) = tokens.next() {
        if token.starts_with('-') {
            if GIT_OPTS_WITH_VALUE.contains(&token) {
                tokens.next();
            }
            continue;
        }
        return Some(token);
    }
    None
}

/// Whether `token` is a leading `NAME=value` environment assignment (a
/// shell-legal variable name followed by `=`), which precedes the command it
/// applies to.
fn is_env_assignment(token: &str) -> bool {
    match token.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name.chars().enumerate().all(|(i, c)| {
                    c == '_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit())
                })
        }
        None => false,
    }
}

/// True when `target` resolves outside `worktree`. A relative `target` is taken
/// relative to `worktree` (the agent's cwd), so it always stays inside; an
/// absolute path, or a relative one that climbs out with `..`, escapes when its
/// normalized form is not under the worktree. Comparison is component-wise, so a
/// sibling sharing a name prefix (`…/sessions/work` vs `…/sessions/work2`) does
/// not count as inside.
pub fn escapes_worktree(worktree: &Path, target: &Path) -> bool {
    let absolute = if target.is_absolute() {
        target.to_path_buf()
    } else {
        worktree.join(target)
    };
    !normalize(&absolute).starts_with(normalize(worktree))
}

/// Fold `.` and `..` out of `path` lexically, without consulting the filesystem.
/// `..` pops the last kept component (and is a no-op at the root), so the result
/// never climbs above the path's own root. Used instead of
/// [`std::fs::canonicalize`] so a not-yet-created file still resolves.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const WT: &str = "/repo/.usagi/sessions/work";

    #[test]
    fn a_file_under_the_worktree_stays_inside() {
        assert!(!escapes_worktree(
            Path::new(WT),
            Path::new("/repo/.usagi/sessions/work/src/main.rs")
        ));
    }

    #[test]
    fn the_worktree_itself_is_inside() {
        assert!(!escapes_worktree(Path::new(WT), Path::new(WT)));
    }

    #[test]
    fn a_relative_path_is_resolved_against_the_worktree_and_stays_inside() {
        assert!(!escapes_worktree(Path::new(WT), Path::new("src/lib.rs")));
    }

    #[test]
    fn an_absolute_path_into_the_parent_repo_escapes() {
        assert!(escapes_worktree(
            Path::new(WT),
            Path::new("/repo/src/main.rs")
        ));
    }

    #[test]
    fn a_relative_path_climbing_out_with_dotdot_escapes() {
        assert!(escapes_worktree(
            Path::new(WT),
            Path::new("../../../src/main.rs")
        ));
    }

    #[test]
    fn dotdot_that_stays_inside_does_not_escape() {
        // `work/src/../Cargo.toml` folds back to `work/Cargo.toml` — still inside.
        assert!(!escapes_worktree(
            Path::new(WT),
            Path::new("src/../Cargo.toml")
        ));
    }

    #[test]
    fn a_sibling_worktree_sharing_a_name_prefix_escapes() {
        // Component-wise containment: `work2` is not under `work` despite the
        // string prefix.
        assert!(escapes_worktree(
            Path::new(WT),
            Path::new("/repo/.usagi/sessions/work2/src/main.rs")
        ));
    }

    #[test]
    fn dotdot_at_the_root_does_not_climb_above_it() {
        // `/..` normalizes to `/`, which is not under the worktree, so it escapes
        // rather than panicking or wrapping.
        assert!(escapes_worktree(Path::new(WT), Path::new("/../etc/passwd")));
    }

    #[test]
    fn normalize_drops_a_leading_current_dir_component() {
        // A leading `.` is the one `CurDir` form `Path::components` preserves
        // (mid-path `.` are already folded away), so normalize it directly: it is
        // skipped, leaving just the real components.
        assert_eq!(normalize(Path::new("./a/b")), PathBuf::from("a/b"));
    }

    #[test]
    fn a_session_worktree_path_is_recognized() {
        assert!(is_session_worktree(Path::new("/repo/.usagi/sessions/work")));
        assert!(is_session_worktree(Path::new(
            "/repo/.usagi/sessions/work/src"
        )));
    }

    #[test]
    fn the_workspace_root_and_unrelated_paths_are_not_session_worktrees() {
        // The coordinator's cwd is the repo root — no `.usagi/sessions` segment.
        assert!(!is_session_worktree(Path::new("/repo")));
        // A `.usagi` dir without the `sessions` child (e.g. the issue store).
        assert!(!is_session_worktree(Path::new("/repo/.usagi/issues")));
        // `sessions` without the `.usagi` parent does not count.
        assert!(!is_session_worktree(Path::new("/repo/sessions/work")));
    }

    #[test]
    fn write_tools_are_recognized_case_sensitively() {
        for tool in ["Write", "Edit", "MultiEdit", "NotebookEdit"] {
            assert!(is_write_tool(tool), "{tool} should be a write tool");
        }
        // Read-only / non-writing tools and Bash are not write tools.
        for tool in ["Read", "Grep", "Glob", "Bash", "Task", "write"] {
            assert!(!is_write_tool(tool), "{tool} should not be a write tool");
        }
    }

    #[test]
    fn mutating_git_commands_are_flagged() {
        for command in [
            "git commit -m 'x'",
            "git add .",
            "git push",
            "git merge main",
            "git rebase main",
            "git checkout -b feat/x",
            "git worktree add ../wt",
            "git reset --hard",
            "git config user.name x",
            "git branch -D old",
        ] {
            assert!(
                command_mutates_repo(command),
                "{command} should be flagged as mutating"
            );
        }
    }

    #[test]
    fn read_only_git_commands_are_allowed() {
        for command in [
            "git status",
            "git log --oneline",
            "git diff HEAD~1",
            "git show abc123",
            "git rev-parse HEAD",
            "git ls-files",
        ] {
            assert!(
                !command_mutates_repo(command),
                "{command} should be allowed"
            );
        }
    }

    #[test]
    fn non_git_commands_are_not_flagged() {
        for command in ["ls -la", "cargo test", "echo hi", ""] {
            assert!(!command_mutates_repo(command));
        }
    }

    #[test]
    fn a_mutating_git_anywhere_in_a_chain_is_flagged() {
        // Read-only leading git does not excuse a later mutating one.
        assert!(command_mutates_repo("git status && git commit -m x"));
        assert!(command_mutates_repo("cd foo; git push"));
        assert!(command_mutates_repo("git log | cat && git add ."));
        // A chain of only read-only git stays allowed.
        assert!(!command_mutates_repo("git status && git log"));
    }

    #[test]
    fn global_options_before_the_subcommand_do_not_hide_it() {
        // `-C <path>` / `-c <cfg>` consume a value token; the subcommand follows.
        assert!(command_mutates_repo("git -C /repo commit -m x"));
        assert!(command_mutates_repo("git -c user.name=x commit"));
        assert!(!command_mutates_repo("git -C /repo status"));
    }

    #[test]
    fn wrappers_and_env_assignments_before_git_are_seen_through() {
        assert!(command_mutates_repo("sudo git push"));
        assert!(command_mutates_repo("GIT_DIR=/x git commit"));
        assert!(command_mutates_repo("env git rebase main"));
        assert!(!command_mutates_repo("FOO=bar git status"));
    }
}

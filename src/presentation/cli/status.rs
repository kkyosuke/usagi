use std::env;

use crate::domain::workspace_state::WorkspaceState;
use crate::usecase::workspace_state;

/// Entry point for `usagi status`: sync the current repository's worktree state
/// to `.usagi/state.json` and print it.
pub fn run() -> anyhow::Result<()> {
    let cwd = env::current_dir()?;
    let state = workspace_state::sync(&cwd)?;
    for line in render(&state) {
        println!("{line}");
    }
    Ok(())
}

/// Formats a [`WorkspaceState`] into the lines printed by `usagi status`.
fn render(state: &WorkspaceState) -> Vec<String> {
    let mut lines = vec![
        format!(
            "default branch: {}  (updated {})",
            state.default_branch,
            state.updated_at.format("%Y-%m-%d %H:%M UTC")
        ),
        String::new(),
    ];
    for wt in &state.worktrees {
        let marker = if wt.primary { "*" } else { " " };
        let branch = wt.branch.as_deref().unwrap_or("(detached)");
        let upstream = wt
            .upstream
            .as_deref()
            .map(|u| format!(" → {u}"))
            .unwrap_or_default();
        lines.push(format!(
            "{marker} {:<8} {:<24} {}{}",
            wt.status.as_str(),
            branch,
            wt.head,
            upstream
        ));
        lines.push(format!("    {}", wt.path.display()));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::workspace_state::{BranchStatus, WorktreeState};
    use crate::infrastructure::git::test_command;
    use chrono::{TimeZone, Utc};
    use std::path::PathBuf;

    fn state() -> WorkspaceState {
        let ts = Utc.with_ymd_and_hms(2026, 6, 13, 5, 1, 0).unwrap();
        WorkspaceState {
            default_branch: "main".to_string(),
            updated_at: ts,
            worktrees: vec![
                WorktreeState {
                    branch: Some("main".to_string()),
                    path: PathBuf::from("/repo"),
                    head: "76e906f".to_string(),
                    primary: true,
                    upstream: Some("origin/main".to_string()),
                    status: BranchStatus::Pushed,
                    updated_at: ts,
                },
                WorktreeState {
                    branch: None,
                    path: PathBuf::from("/repo/wt"),
                    head: "aaf5459".to_string(),
                    primary: false,
                    upstream: None,
                    status: BranchStatus::Local,
                    updated_at: ts,
                },
            ],
        }
    }

    #[test]
    fn render_marks_primary_upstream_and_detached_head() {
        let lines = render(&state());
        assert_eq!(
            lines[0],
            "default branch: main  (updated 2026-06-13 05:01 UTC)"
        );
        assert_eq!(lines[1], "");
        // Primary worktree: leading "*", status, branch, head and upstream.
        assert_eq!(
            lines[2],
            "* pushed   main                     76e906f → origin/main"
        );
        assert_eq!(lines[3], "    /repo");
        // Secondary worktree: leading space, detached label, no upstream arrow.
        assert_eq!(lines[4], "  local    (detached)               aaf5459");
        assert_eq!(lines[5], "    /repo/wt");
    }

    #[test]
    fn run_syncs_and_prints_for_the_current_repo() {
        // `run` reads the current directory, so point it at a throwaway repo.
        let dir = tempfile::tempdir().unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@e.com"][..],
            &["config", "user.name", "t"][..],
        ] {
            test_command(dir.path()).args(args).status().unwrap();
        }
        std::fs::write(dir.path().join("f"), "x").unwrap();
        for args in [&["add", "."][..], &["commit", "-q", "-m", "i"][..]] {
            test_command(dir.path()).args(args).status().unwrap();
        }

        let original = env::current_dir().unwrap();
        env::set_current_dir(dir.path()).unwrap();
        let result = run();
        env::set_current_dir(original).unwrap();

        assert!(result.is_ok());
    }
}

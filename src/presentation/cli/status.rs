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

/// Formats a [`WorkspaceState`] into the lines printed by `usagi status`:
/// one block per session, listing each repository's worktree and its status.
fn render(state: &WorkspaceState) -> Vec<String> {
    let mut lines = vec![
        format!("updated {}", state.updated_at.format("%Y-%m-%d %H:%M UTC")),
        String::new(),
    ];

    if state.sessions.is_empty() {
        lines.push("No sessions yet. Run \"session create <name>\" to create one.".to_string());
        return lines;
    }

    for session in &state.sessions {
        lines.push(format!(
            "session \"{}\"  ({})",
            session.name,
            session.root.display()
        ));
        for wt in &session.worktrees {
            let branch = wt.branch.as_deref().unwrap_or("(detached)");
            let upstream = wt
                .upstream
                .as_deref()
                .map(|u| format!(" → {u}"))
                .unwrap_or_default();
            lines.push(format!(
                "  {:<8} {:<24} {}{}",
                wt.status.as_str(),
                branch,
                wt.head,
                upstream
            ));
            lines.push(format!("    {}", wt.path.display()));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::workspace_state::{BranchStatus, SessionRecord, WorktreeState};
    use crate::infrastructure::git::test_command;
    use chrono::{TimeZone, Utc};
    use std::path::PathBuf;

    fn state() -> WorkspaceState {
        let ts = Utc.with_ymd_and_hms(2026, 6, 13, 5, 1, 0).unwrap();
        WorkspaceState {
            updated_at: ts,
            sessions: vec![SessionRecord {
                name: "login".to_string(),
                display_name: None,
                root: PathBuf::from("/repo/.usagi/sessions/login"),
                created_at: ts,
                worktrees: vec![
                    WorktreeState {
                        branch: Some("login".to_string()),
                        path: PathBuf::from("/repo/.usagi/sessions/login/app-a"),
                        head: "76e906f".to_string(),
                        primary: false,
                        upstream: Some("origin/login".to_string()),
                        status: BranchStatus::Pushed,
                        updated_at: ts,
                    },
                    WorktreeState {
                        branch: None,
                        path: PathBuf::from("/repo/.usagi/sessions/login/app-b"),
                        head: "aaf5459".to_string(),
                        primary: false,
                        upstream: None,
                        status: BranchStatus::Local,
                        updated_at: ts,
                    },
                ],
            }],
        }
    }

    #[test]
    fn render_lists_sessions_with_their_worktrees() {
        let lines = render(&state());
        assert_eq!(lines[0], "updated 2026-06-13 05:01 UTC");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "session \"login\"  (/repo/.usagi/sessions/login)");
        // First worktree: status, branch, head and upstream arrow.
        assert_eq!(
            lines[3],
            "  pushed   login                    76e906f → origin/login"
        );
        assert_eq!(lines[4], "    /repo/.usagi/sessions/login/app-a");
        // Second worktree: detached label, no upstream arrow.
        assert_eq!(lines[5], "  local    (detached)               aaf5459");
        assert_eq!(lines[6], "    /repo/.usagi/sessions/login/app-b");
    }

    #[test]
    fn render_reports_when_there_are_no_sessions() {
        let mut s = state();
        s.sessions.clear();
        let lines = render(&s);
        assert!(lines.last().unwrap().contains("No sessions yet"));
    }

    #[test]
    fn run_syncs_and_prints_for_the_current_repo() {
        // `run` reads the current directory, so point it at a throwaway repo.
        // The guard serializes the cwd change against other globals-mutating tests.
        let _guard = crate::test_support::process_env_guard();
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

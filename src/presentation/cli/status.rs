use std::env;

use crate::domain::workspace_state::WorkspaceState;
use crate::usecase::workspace_state;

/// Entry point for `usagi status`: sync the current repository's worktree state
/// to `.usagi/state.json` and print it.
pub fn run() -> anyhow::Result<()> {
    let cwd = env::current_dir()?;
    let state = workspace_state::sync(&cwd)?;
    print_state(&state);
    Ok(())
}

fn print_state(state: &WorkspaceState) {
    println!(
        "default branch: {}  (updated {})",
        state.default_branch,
        state.updated_at.format("%Y-%m-%d %H:%M UTC")
    );
    println!();
    for wt in &state.worktrees {
        let marker = if wt.primary { "*" } else { " " };
        let branch = wt.branch.as_deref().unwrap_or("(detached)");
        let upstream = wt
            .upstream
            .as_deref()
            .map(|u| format!(" → {u}"))
            .unwrap_or_default();
        println!(
            "{marker} {:<8} {:<24} {}{}",
            wt.status.as_str(),
            branch,
            wt.head,
            upstream
        );
        println!("    {}", wt.path.display());
    }
}

//! The hidden `usagi agent-phase <phase>` subcommand.
//!
//! It is never run by a person: usagi wires it into the agent CLI as a set of
//! lifecycle hooks (see [`crate::domain::settings::AgentCli::launch_command`]),
//! so the agent itself reports each transition. The hook delivers its JSON
//! payload on stdin; usagi reads the worktree (`cwd`) from it and records the
//! phase so the home screen's session watcher can show the session as running
//! or waiting. This is a thin stdin → file shim; its file-path and JSON logic
//! live (and are tested) in [`crate::infrastructure::agent_state_store`].

use std::io::Read;

use anyhow::Result;
use clap::ValueEnum;

use crate::domain::agent_phase::AgentPhase;
use crate::infrastructure::agent_state_store;

/// The phase a hook reports, as accepted on the command line.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Phase {
    /// A turn started (the `UserPromptSubmit` hook).
    Running,
    /// A turn ended or paused for input (the `Stop` / `Notification` hooks).
    Waiting,
    /// The agent exited (the `SessionEnd` hook).
    Ended,
}

impl From<Phase> for AgentPhase {
    fn from(phase: Phase) -> Self {
        match phase {
            Phase::Running => AgentPhase::Running,
            Phase::Waiting => AgentPhase::Waiting,
            Phase::Ended => AgentPhase::Ended,
        }
    }
}

/// Entry point for `usagi agent-phase <phase>`. Reads the hook payload from
/// stdin to learn which worktree fired, then records `phase` for it. Falls back
/// to the process's current directory when stdin carries no usable `cwd`.
pub fn run(phase: Phase) -> Result<()> {
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let worktree = match agent_state_store::worktree_from_hook_json(&raw) {
        Some(worktree) => worktree,
        None => std::env::current_dir()?,
    };
    agent_state_store::write(&worktree, phase.into())
}

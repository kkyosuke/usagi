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
    /// The session just started or resumed and is idle (the `SessionStart` hook).
    Ready,
    /// A turn started (the `UserPromptSubmit` hook).
    Running,
    /// The agent paused mid-turn for the user's input or permission (the
    /// `Notification` hook).
    Waiting,
    /// The agent finished — a turn ended (`Stop`) or the process exited
    /// (`SessionEnd`).
    Ended,
}

impl From<Phase> for AgentPhase {
    fn from(phase: Phase) -> Self {
        match phase {
            Phase::Ready => AgentPhase::Ready,
            Phase::Running => AgentPhase::Running,
            Phase::Waiting => AgentPhase::Waiting,
            Phase::Ended => AgentPhase::Ended,
        }
    }
}

/// Entry point for `usagi agent-phase <phase>`. Reads the hook payload from
/// stdin to learn which worktree fired, then records `phase` for it. Falls back
/// to the process's current directory when stdin carries no usable `cwd`.
///
/// One exception keeps a busy session from being shown idle: the `SessionStart`
/// hook normally records `ready`, but `SessionStart` also fires **mid-turn**
/// after a context compaction — the agent keeps working afterwards with no fresh
/// `UserPromptSubmit` to put it back to `running`, so recording `ready` then
/// would strand the session showing ready (`☾`) while it works, until its next
/// `Stop`. [`agent_state_store::ready_overwrite_allowed`] decides whether this
/// `ready` is a genuine idle start (recorded) or a mid-turn restart (skipped,
/// preserving whatever phase the session was already in) — keyed off both the
/// hook `source` and the phase currently recorded for the worktree.
pub fn run(phase: Phase) -> Result<()> {
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let worktree = match agent_state_store::worktree_from_hook_json(&raw) {
        Some(worktree) => worktree,
        None => std::env::current_dir()?,
    };
    if matches!(phase, Phase::Ready)
        && !agent_state_store::ready_overwrite_allowed(
            agent_state_store::read(&worktree),
            agent_state_store::session_start_source_from_hook_json(&raw).as_deref(),
        )
    {
        return Ok(());
    }
    agent_state_store::write(&worktree, phase.into())
}

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
use std::path::Path;

use anyhow::Result;
use clap::ValueEnum;

use crate::domain::agent_phase::AgentPhase;
use crate::infrastructure::agent_state_store;
use crate::usecase::agent_phase as agent_phase_policy;

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
    /// The worker process exited unsuccessfully.
    Failed,
    /// The worker was explicitly interrupted.
    Interrupted,
    /// The worker exceeded its orchestration deadline.
    TimedOut,
}

impl From<Phase> for AgentPhase {
    fn from(phase: Phase) -> Self {
        match phase {
            Phase::Ready => AgentPhase::Ready,
            Phase::Running => AgentPhase::Running,
            Phase::Waiting => AgentPhase::Waiting,
            Phase::Ended => AgentPhase::Ended,
            Phase::Failed | Phase::Interrupted | Phase::TimedOut => AgentPhase::Ended,
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
/// `Stop`. [`agent_phase_policy::ready_overwrite_allowed`] decides whether this
/// `ready` is a genuine idle start (recorded) or a mid-turn restart (skipped,
/// preserving whatever phase the session was already in) — keyed off both the
/// hook `source` and the phase currently recorded for the worktree.
pub fn run(phase: Phase, mut input: impl Read) -> Result<()> {
    // The hook payload arrives on `input` (stdin in production); reading it is the
    // caller's only IO, injected so the whole transition is unit-tested without
    // touching — or blocking on — the process's real stdin.
    let mut raw = String::new();
    let _ = input.read_to_string(&mut raw);
    let worktree = match agent_state_store::worktree_from_hook_json(&raw) {
        Some(worktree) => worktree,
        None => std::env::current_dir()?,
    };
    record(phase, &raw, &worktree)
}

/// Record `phase` for `worktree`, applying two transition guards: the
/// `SessionStart` → `ready` guard so a mid-turn restart never strands a working
/// session showing idle, and the `Notification` → `waiting` guard so Claude's
/// post-`Stop` idle notification never flips a finished session back to waiting.
/// The decisions are [`agent_phase_policy::ready_overwrite_allowed`]'s and
/// [`agent_phase_policy::waiting_overwrite_allowed`]'s; this wires them to the
/// recorded phase and the hook `source` parsed from `raw`. Split from [`run`] so
/// the transition logic is unit-tested without the stdin / `cwd` IO.
fn record(phase: Phase, raw: &str, worktree: &Path) -> Result<()> {
    if matches!(phase, Phase::Ready)
        && !agent_phase_policy::ready_overwrite_allowed(
            agent_state_store::read(worktree),
            agent_state_store::session_start_source_from_hook_json(raw).as_deref(),
        )
    {
        return Ok(());
    }
    // A `Notification` → `waiting` that lands on a finished (`Ended`) session is
    // Claude's post-`Stop` idle notification; recording it would flip the ✓ back
    // to waiting. Skip it, preserving the `ended` phase (see
    // [`agent_phase_policy::waiting_overwrite_allowed`]).
    if matches!(phase, Phase::Waiting)
        && !agent_phase_policy::waiting_overwrite_allowed(agent_state_store::read(worktree))
    {
        return Ok(());
    }
    agent_state_store::write(worktree, phase.into())?;
    let event_kind = match phase {
        Phase::Ended => Some(crate::domain::orchestrator::EventKind::Succeeded),
        Phase::Failed => Some(crate::domain::orchestrator::EventKind::Failed),
        Phase::Interrupted => Some(crate::domain::orchestrator::EventKind::Interrupted),
        Phase::TimedOut => Some(crate::domain::orchestrator::EventKind::TimedOut),
        Phase::Ready | Phase::Running | Phase::Waiting => None,
    };
    if let Some(kind) = event_kind {
        crate::infrastructure::orchestrator_event::emit(worktree, kind, 0, chrono::Utc::now())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::storage;
    use std::io::Cursor;

    /// Point the agent-state store at a throwaway data dir for the duration of
    /// `body`, serialized against other env-mutating tests.
    fn with_data_dir(body: impl FnOnce(&Path)) {
        let _guard = crate::test_support::process_env_guard();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(storage::DATA_DIR_ENV, dir.path());
        body(dir.path());
        std::env::remove_var(storage::DATA_DIR_ENV);
    }

    #[test]
    fn phase_converts_to_its_domain_counterpart() {
        assert_eq!(AgentPhase::from(Phase::Ready), AgentPhase::Ready);
        assert_eq!(AgentPhase::from(Phase::Running), AgentPhase::Running);
        assert_eq!(AgentPhase::from(Phase::Waiting), AgentPhase::Waiting);
        assert_eq!(AgentPhase::from(Phase::Ended), AgentPhase::Ended);
        assert_eq!(AgentPhase::from(Phase::Failed), AgentPhase::Ended);
        assert_eq!(AgentPhase::from(Phase::Interrupted), AgentPhase::Ended);
        assert_eq!(AgentPhase::from(Phase::TimedOut), AgentPhase::Ended);
    }

    #[test]
    fn run_records_the_worktree_from_the_hook_payload() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let payload = format!("{{\"cwd\":{:?}}}", wt.path().to_str().unwrap());
            run(Phase::Running, Cursor::new(payload)).unwrap();
            assert_eq!(
                agent_state_store::read(wt.path()),
                Some(AgentPhase::Running)
            );
        });
    }

    #[test]
    fn run_records_ready_for_a_fresh_session() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let payload = format!("{{\"cwd\":{:?}}}", wt.path().to_str().unwrap());
            // No phase recorded yet and no `compact` source: `ready` is written.
            run(Phase::Ready, Cursor::new(payload)).unwrap();
            assert_eq!(agent_state_store::read(wt.path()), Some(AgentPhase::Ready));
        });
    }

    #[test]
    fn run_skips_ready_when_a_turn_is_already_in_progress() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let cwd = wt.path().to_str().unwrap();
            let payload = format!("{{\"cwd\":{cwd:?}}}");
            run(Phase::Running, Cursor::new(payload.clone())).unwrap();
            // A mid-turn `SessionStart` → ready must not reset the running phase.
            run(Phase::Ready, Cursor::new(payload)).unwrap();
            assert_eq!(
                agent_state_store::read(wt.path()),
                Some(AgentPhase::Running)
            );
        });
    }

    #[test]
    fn run_skips_ready_on_a_compaction_restart() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let cwd = wt.path().to_str().unwrap();
            // A `compact` source means this `SessionStart` is a mid-turn restart;
            // nothing is recorded, so the phase file stays absent.
            let payload = format!("{{\"cwd\":{cwd:?},\"source\":\"compact\"}}");
            run(Phase::Ready, Cursor::new(payload)).unwrap();
            assert_eq!(agent_state_store::read(wt.path()), None);
        });
    }

    #[test]
    fn run_skips_waiting_over_a_finished_session() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let cwd = wt.path().to_str().unwrap();
            let payload = format!("{{\"cwd\":{cwd:?}}}");
            // A turn ended, then Claude's idle `Notification` → waiting fires:
            // the finished (`Ended`) phase must survive rather than flip to waiting.
            run(Phase::Ended, Cursor::new(payload.clone())).unwrap();
            run(Phase::Waiting, Cursor::new(payload)).unwrap();
            assert_eq!(agent_state_store::read(wt.path()), Some(AgentPhase::Ended));
        });
    }

    #[test]
    fn run_records_waiting_over_a_running_session() {
        with_data_dir(|_| {
            let wt = tempfile::tempdir().unwrap();
            let cwd = wt.path().to_str().unwrap();
            let payload = format!("{{\"cwd\":{cwd:?}}}");
            // A genuine mid-turn pause: running → waiting is recorded.
            run(Phase::Running, Cursor::new(payload.clone())).unwrap();
            run(Phase::Waiting, Cursor::new(payload)).unwrap();
            assert_eq!(
                agent_state_store::read(wt.path()),
                Some(AgentPhase::Waiting)
            );
        });
    }

    #[test]
    fn run_falls_back_to_the_current_dir_without_a_cwd_in_the_payload() {
        with_data_dir(|_| {
            // An empty payload carries no `cwd`, so `run` records for the process's
            // current directory instead of erroring.
            run(Phase::Ended, Cursor::new(String::new())).unwrap();
            let cwd = std::env::current_dir().unwrap();
            assert_eq!(agent_state_store::read(&cwd), Some(AgentPhase::Ended));
        });
    }
}

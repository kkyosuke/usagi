//! Agent lifecycle phase transition policy.
//!
//! When an agent's `SessionStart` hook fires, usagi must decide whether to
//! record the resulting `ready` phase or leave the worktree's current phase
//! intact. That decision is a state-transition policy, not IO: the hook-payload
//! parsing and the on-disk phase file live in
//! [`crate::infrastructure::agent_state_store`], while the rule for *whether* a
//! transition is allowed lives here. The hidden `usagi agent-phase` subcommand
//! ([`crate::presentation::cli::agent_phase`]) wires the two together.

use crate::domain::agent_phase::AgentPhase;

/// Whether a `SessionStart` → `ready` hook should actually be recorded for a
/// worktree whose currently recorded phase is `current` and whose hook payload
/// reports `source`.
///
/// `SessionStart` fires at a genuine idle start (`source` `startup` / `resume` /
/// `clear`) but **also mid-turn** after a context compaction, after which the
/// agent keeps working with no fresh `UserPromptSubmit` to put it back to
/// `running`. Recording `ready` then strands a still-working session showing
/// idle (`☾`) until its next `Stop`. We refuse the write when **either**:
///
/// - the source is `compact` — an explicit compaction restart (this is what
///   [#171] fixed); or
/// - the recorded phase is already `Running`/`Waiting` — the session is mid-turn,
///   so this `SessionStart` is a restart, not a genuine idle start. This guard is
///   robust even when a compaction does *not* carry a `compact` source (a Claude
///   version that compacts without a fresh `SessionStart`, or a payload whose
///   `source` could not be read), which the source check alone would miss.
///
/// This stays correct for genuine starts because usagi clears the phase file on
/// every fresh spawn (see [`crate::infrastructure::agent_state_store::clear`] and
/// [`crate::presentation::tui::home::terminal::pool::TerminalPool::attach_or_spawn`]):
/// a `startup` / `resume` / `clear` always finds no in-progress phase and is
/// recorded normally.
///
/// [#171]: https://github.com/KKyosuke/usagi/pull/171
pub fn ready_overwrite_allowed(current: Option<AgentPhase>, source: Option<&str>) -> bool {
    source != Some("compact") && !matches!(current, Some(AgentPhase::Running | AgentPhase::Waiting))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_is_recorded_for_a_genuine_idle_start() {
        // A fresh spawn has cleared the phase file, so a startup / resume / clear
        // SessionStart finds no in-progress phase and is recorded as ready.
        assert!(ready_overwrite_allowed(None, Some("startup")));
        assert!(ready_overwrite_allowed(None, Some("resume")));
        assert!(ready_overwrite_allowed(None, Some("clear")));
        // Re-recording ready over an already-idle session is fine.
        assert!(ready_overwrite_allowed(
            Some(AgentPhase::Ready),
            Some("startup")
        ));
        assert!(ready_overwrite_allowed(
            Some(AgentPhase::Ended),
            Some("clear")
        ));
    }

    #[test]
    fn ready_is_skipped_for_a_compaction_restart() {
        // A `compact` source is an explicit mid-turn compaction: never reset to
        // ready, whatever phase the session was in.
        assert!(!ready_overwrite_allowed(
            Some(AgentPhase::Running),
            Some("compact")
        ));
        assert!(!ready_overwrite_allowed(
            Some(AgentPhase::Ended),
            Some("compact")
        ));
        assert!(!ready_overwrite_allowed(None, Some("compact")));
    }

    #[test]
    fn ready_is_skipped_mid_turn_even_without_a_compact_source() {
        // The robust guard: a session recorded as running / waiting is mid-turn,
        // so a SessionStart there is a restart (a compaction that carried no
        // `compact` source, or a payload whose source could not be read) — not a
        // genuine idle start. Preserve the real phase.
        assert!(!ready_overwrite_allowed(
            Some(AgentPhase::Running),
            Some("resume")
        ));
        assert!(!ready_overwrite_allowed(Some(AgentPhase::Waiting), None));
        assert!(!ready_overwrite_allowed(Some(AgentPhase::Running), None));
    }
}

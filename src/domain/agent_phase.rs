use serde::{Deserialize, Serialize};

/// The hidden CLI subcommand used by lifecycle hooks to report an agent phase.
pub const AGENT_PHASE_COMMAND: &str = "agent-phase";

const READY_NAME: &str = "ready";
const RUNNING_NAME: &str = "running";
const WAITING_NAME: &str = "waiting";
const ENDED_NAME: &str = "ended";

/// The lifecycle phase of an agent CLI (e.g. Claude Code) running inside a
/// session's embedded shell, as reported by the agent's own lifecycle hooks.
///
/// usagi launches the agent with hooks that report each transition (see
/// [`crate::domain::settings::AgentCli::launch_command`]). Each hook writes the
/// new phase to a small per-worktree file, which the home screen's session
/// watcher reads back to drive the ready / running / waiting indicator. Agents
/// without such hooks — or before their first hook fires — report no phase at
/// all, and usagi falls back to its terminal-bell heuristic
/// ([`crate::infrastructure::session_monitor`]).
///
/// The phases follow the agent's turn lifecycle: a freshly started (or resumed)
/// session is [`Ready`](Self::Ready); submitting a prompt makes it
/// [`Running`](Self::Running); pausing mid-turn for the user's input or
/// permission makes it [`Waiting`](Self::Waiting); finishing a turn or the
/// process exiting makes it [`Ended`](Self::Ended).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPhase {
    /// The agent has just started (or resumed) and is idle, awaiting the user's
    /// first prompt — it has not begun a turn yet.
    Ready,
    /// The agent is actively working a turn: a prompt was submitted and it is
    /// generating a response or running tools.
    Running,
    /// The agent paused mid-turn and is waiting for the user's input or
    /// permission (it asked something or needs a tool approved).
    Waiting,
    /// The agent finished: it completed a turn (`Stop`) or its process exited
    /// (`SessionEnd`). The bare shell it launched in may still be alive.
    Ended,
}

impl AgentPhase {
    /// The lowercase name of this phase (`ready` / `running` / `waiting` /
    /// `ended`), matching its serde representation.
    ///
    /// Used where a phase is surfaced as a plain string rather than serialized
    /// through serde — e.g. the `session_status` MCP tool, which reports a
    /// worktree with no recorded phase as `none` and otherwise this name.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentPhase::Ready => READY_NAME,
            AgentPhase::Running => RUNNING_NAME,
            AgentPhase::Waiting => WAITING_NAME,
            AgentPhase::Ended => ENDED_NAME,
        }
    }

    /// Whether this phase authoritatively decides the displayed state.
    ///
    /// [`Ready`](Self::Ready), [`Running`](Self::Running) and
    /// [`Waiting`](Self::Waiting) come straight from the agent and override the
    /// bell heuristic; [`Ended`](Self::Ended) means the agent is gone, so the
    /// state falls back to the bare shell's bell.
    pub fn is_active(self) -> bool {
        matches!(
            self,
            AgentPhase::Ready | AgentPhase::Running | AgentPhase::Waiting
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_in_snake_case() {
        assert_eq!(
            serde_json::to_string(&AgentPhase::Ready).unwrap(),
            "\"ready\""
        );
        assert_eq!(
            serde_json::to_string(&AgentPhase::Running).unwrap(),
            "\"running\""
        );
        assert_eq!(
            serde_json::to_string(&AgentPhase::Waiting).unwrap(),
            "\"waiting\""
        );
        assert_eq!(
            serde_json::to_string(&AgentPhase::Ended).unwrap(),
            "\"ended\""
        );
    }

    #[test]
    fn deserializes_from_snake_case() {
        assert_eq!(
            serde_json::from_str::<AgentPhase>("\"waiting\"").unwrap(),
            AgentPhase::Waiting
        );
    }

    #[test]
    fn deserializes_ready_from_snake_case() {
        assert_eq!(
            serde_json::from_str::<AgentPhase>("\"ready\"").unwrap(),
            AgentPhase::Ready
        );
    }

    #[test]
    fn as_str_matches_the_serde_name() {
        // The plain-string name mirrors the snake_case serde representation, so
        // the two never drift.
        for phase in [
            AgentPhase::Ready,
            AgentPhase::Running,
            AgentPhase::Waiting,
            AgentPhase::Ended,
        ] {
            assert_eq!(
                serde_json::to_string(&phase).unwrap(),
                format!("\"{}\"", phase.as_str())
            );
        }
    }

    #[test]
    fn every_phase_but_ended_is_active() {
        assert!(AgentPhase::Ready.is_active());
        assert!(AgentPhase::Running.is_active());
        assert!(AgentPhase::Waiting.is_active());
        assert!(!AgentPhase::Ended.is_active());
    }
}

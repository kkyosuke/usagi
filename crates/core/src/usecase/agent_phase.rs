//! Shared Agent phase aggregation for daemon and TUI projections.

use crate::domain::session_lifecycle::AgentPhase;

/// The user-facing aggregation class of one Agent phase.
///
/// This ordering is the single source used when multiple runtimes are folded
/// into a session or Home target projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentPhaseAggregation {
    Absent,
    Ready,
    Running,
    Waiting,
    Done,
}

impl AgentPhaseAggregation {
    /// Folds the closed Agent phase vocabulary into its aggregation class.
    #[must_use]
    pub const fn from_phase(phase: AgentPhase) -> Self {
        match phase {
            AgentPhase::Absent => Self::Absent,
            AgentPhase::Ready => Self::Ready,
            AgentPhase::Running => Self::Running,
            AgentPhase::Waiting => Self::Waiting,
            AgentPhase::Ended | AgentPhase::Exited | AgentPhase::Interrupted => Self::Done,
        }
    }

    /// Returns the shared `Done > Waiting > Running > Ready > Absent` rank.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Absent => 0,
            Self::Ready => 1,
            Self::Running => 2,
            Self::Waiting => 3,
            Self::Done => 4,
        }
    }
}

/// Returns the shared aggregation rank for one phase.
#[must_use]
pub const fn agent_phase_aggregation_rank(phase: AgentPhase) -> u8 {
    AgentPhaseAggregation::from_phase(phase).rank()
}

#[cfg(test)]
mod tests {
    use super::{AgentPhaseAggregation, agent_phase_aggregation_rank};
    use crate::domain::session_lifecycle::AgentPhase;

    #[test]
    fn aggregation_folds_every_closed_phase_into_the_shared_order() {
        for (phase, class, rank) in [
            (AgentPhase::Absent, AgentPhaseAggregation::Absent, 0),
            (AgentPhase::Ready, AgentPhaseAggregation::Ready, 1),
            (AgentPhase::Running, AgentPhaseAggregation::Running, 2),
            (AgentPhase::Waiting, AgentPhaseAggregation::Waiting, 3),
            (AgentPhase::Ended, AgentPhaseAggregation::Done, 4),
            (AgentPhase::Exited, AgentPhaseAggregation::Done, 4),
            (AgentPhase::Interrupted, AgentPhaseAggregation::Done, 4),
        ] {
            assert_eq!(AgentPhaseAggregation::from_phase(phase), class);
            assert_eq!(agent_phase_aggregation_rank(phase), rank);
        }
    }
}

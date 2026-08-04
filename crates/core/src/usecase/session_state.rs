//! Session の状態別分類（running / waiting / failed）と、その件数集計。
//!
//! 分類は既存語彙だけで決まる。物理的な可用性は
//! [`SessionLifecycle`]、runtime の活動は
//! [`AgentPhaseAggregation`](crate::usecase::agent_phase::AgentPhaseAggregation)
//! が権威であり、この module は 2 つを 1 つの表示クラスへ畳むだけの純粋な関数を持つ。
//! 新しい状態語彙も、集計値の永続化・wire 表現も持たない（件数は常に権威 projection から
//! 導出する）。

use crate::domain::session_lifecycle::SessionLifecycle;
use crate::usecase::agent_phase::AgentPhaseAggregation;

/// 1 つの managed session が属する表示クラス。
///
/// クラスは排他であり、1 session はちょうど 1 つに属する。したがって
/// [`SessionStateCounts`] の 3 つの件数の合計が session 数を超えることはない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStateClass {
    /// `Failed` lifecycle。作成・削除が失敗して使えない checkout。
    Failed,
    /// 少なくとも 1 つの runtime が入力待ち。
    Waiting,
    /// 少なくとも 1 つの runtime が実行中。
    Running,
    /// 上記以外（runtime 不在・ready・終了済み）。件数には計上しない。
    Other,
}

impl SessionStateClass {
    /// lifecycle と Agent phase 集約から表示クラスを決める。
    ///
    /// 優先順位は `Failed > Waiting > Running` である。`Failed` は lifecycle だけが
    /// 権威で、Agent phase の `Done`（`ended` / `exited` / `interrupted` の畳み込み）を
    /// 失敗として扱わない。`interrupted` は daemon 再起動後に runtime identity を
    /// 証明できなかった daemon 所有の projection 状態であり、checkout が壊れた
    /// `Failed` とは別の事実である。
    #[must_use]
    pub const fn classify(lifecycle: SessionLifecycle, phase: AgentPhaseAggregation) -> Self {
        match (lifecycle, phase) {
            (SessionLifecycle::Failed, _) => Self::Failed,
            (_, AgentPhaseAggregation::Waiting) => Self::Waiting,
            (_, AgentPhaseAggregation::Running) => Self::Running,
            (
                SessionLifecycle::Creating
                | SessionLifecycle::Initializing
                | SessionLifecycle::Available
                | SessionLifecycle::Deleting,
                AgentPhaseAggregation::Absent
                | AgentPhaseAggregation::Ready
                | AgentPhaseAggregation::Done,
            ) => Self::Other,
        }
    }
}

/// workspace の session を状態別に数えた件数。
///
/// 権威 projection から毎回導出する派生値であり、persist も serialize もしない。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionStateCounts {
    /// 実行中の session 数。
    pub running: usize,
    /// 入力待ちの session 数。
    pub waiting: usize,
    /// 失敗した session 数。
    pub failed: usize,
}

impl SessionStateCounts {
    /// `sessions`（lifecycle と Agent phase 集約の組）を状態別に数える。
    ///
    /// 引数は slice で受ける。generic な iterator にすると単相化ごとに coverage が
    /// 分かれるためである。
    #[must_use]
    pub fn tally(sessions: &[(SessionLifecycle, AgentPhaseAggregation)]) -> Self {
        let mut counts = Self::default();
        for (lifecycle, phase) in sessions {
            match SessionStateClass::classify(*lifecycle, *phase) {
                SessionStateClass::Failed => counts.failed += 1,
                SessionStateClass::Waiting => counts.waiting += 1,
                SessionStateClass::Running => counts.running += 1,
                SessionStateClass::Other => {}
            }
        }
        counts
    }

    /// 表示すべき件数が 1 つも無いか。session 0 件、または全員が非計上クラスのとき真。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.running == 0 && self.waiting == 0 && self.failed == 0
    }
}

#[cfg(test)]
mod tests {
    use super::{SessionStateClass, SessionStateCounts};
    use crate::domain::session_lifecycle::{AgentPhase, SessionLifecycle};
    use crate::usecase::agent_phase::AgentPhaseAggregation;

    /// 表示クラスは lifecycle と phase 集約の組だけで決まる。`Failed` は lifecycle が
    /// 権威で、phase 側の `Done` に畳まれる `ended` / `exited` / `interrupted` は
    /// 失敗ではない。
    #[test]
    fn classification_follows_the_documented_precedence() {
        use AgentPhaseAggregation as Phase;
        use SessionLifecycle as Life;

        for phase in [
            Phase::Absent,
            Phase::Ready,
            Phase::Running,
            Phase::Waiting,
            Phase::Done,
        ] {
            // Failed lifecycle は phase を問わず failed。
            assert_eq!(
                SessionStateClass::classify(Life::Failed, phase),
                SessionStateClass::Failed,
                "failed lifecycle with {phase:?}"
            );
        }

        for lifecycle in [
            Life::Creating,
            Life::Initializing,
            Life::Available,
            Life::Deleting,
        ] {
            assert_eq!(
                SessionStateClass::classify(lifecycle, Phase::Waiting),
                SessionStateClass::Waiting
            );
            assert_eq!(
                SessionStateClass::classify(lifecycle, Phase::Running),
                SessionStateClass::Running
            );
            for quiet in [Phase::Absent, Phase::Ready, Phase::Done] {
                assert_eq!(
                    SessionStateClass::classify(lifecycle, quiet),
                    SessionStateClass::Other,
                    "{lifecycle:?} with {quiet:?}"
                );
            }
        }
    }

    /// `interrupted` / `exited` / `ended` は `Done` へ畳まれ、failed には数えない。
    #[test]
    fn ended_exited_and_interrupted_are_not_failures() {
        for phase in [
            AgentPhase::Ended,
            AgentPhase::Exited,
            AgentPhase::Interrupted,
        ] {
            let counts = SessionStateCounts::tally(&[(
                SessionLifecycle::Available,
                AgentPhaseAggregation::from_phase(phase),
            )]);
            assert_eq!(counts, SessionStateCounts::default(), "{phase:?}");
            assert!(counts.is_empty());
        }
    }

    #[test]
    fn tally_counts_each_class_exactly_once() {
        use AgentPhaseAggregation as Phase;
        use SessionLifecycle as Life;

        let counts = SessionStateCounts::tally(&[
            (Life::Available, Phase::Running),
            (Life::Available, Phase::Running),
            (Life::Available, Phase::Waiting),
            (Life::Failed, Phase::Absent),
            // A failed session whose runtime phase has not been dropped yet is
            // still counted once, as a failure.
            (Life::Failed, Phase::Running),
            (Life::Available, Phase::Ready),
            (Life::Creating, Phase::Absent),
        ]);
        assert_eq!(
            counts,
            SessionStateCounts {
                running: 2,
                waiting: 1,
                failed: 2,
            }
        );
        assert!(!counts.is_empty());
        // The classes are exclusive, so the total never exceeds the input size.
        assert!(counts.running + counts.waiting + counts.failed <= 7);
    }

    #[test]
    fn an_empty_workspace_tallies_to_nothing() {
        let counts = SessionStateCounts::tally(&[]);
        assert_eq!(counts, SessionStateCounts::default());
        assert!(counts.is_empty());
        assert!(format!("{counts:?}").contains("running"));
        assert_eq!(counts, counts.clone());
    }
}

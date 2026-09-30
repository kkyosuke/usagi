//! Optional decision guidance and enforceable multiple-choice cardinality.

use serde::{Deserialize, Serialize};

use super::{UserDecision, UserDecisionError, UserDecisionPolicy, UserDecisionSelectionMode};
use crate::domain::presentation_text::presentation_character_is_safe;

/// An explanation, never an automatically selected or submitted answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserDecisionRecommendation {
    pub option_ids: Vec<String>,
    pub reason: String,
}

/// Inclusive selection bounds. Freeform remains a separate answer mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserDecisionSelectionLimits {
    pub min: usize,
    pub max: usize,
}

impl UserDecision {
    /// Effective multiple-choice limits, retaining the legacy one-or-more default.
    #[must_use]
    pub fn selection_bounds(&self) -> (usize, usize) {
        self.selection_limits
            .map_or((1, self.options.len()), |limits| (limits.min, limits.max))
    }

    pub(super) fn validate_guidance(&self) -> Result<(), UserDecisionError> {
        if let Some(limits) = self.selection_limits
            && (self.selection_mode != UserDecisionSelectionMode::Multiple
                || limits.min == 0
                || limits.min > limits.max
                || limits.max > self.options.len())
        {
            return Err(UserDecisionError::InvalidRequest);
        }
        if let Some(recommendation) = &self.recommendation {
            let (min, max) = match self.selection_mode {
                UserDecisionSelectionMode::Single => (1, 1),
                UserDecisionSelectionMode::Multiple => self.selection_bounds(),
            };
            let count = recommendation.option_ids.len();
            if count < min
                || count > max
                || recommendation.reason.trim().is_empty()
                || recommendation.reason.len() > UserDecisionPolicy::RECOMMENDATION_REASON_MAX_BYTES
                || !recommendation
                    .reason
                    .chars()
                    .all(|ch| ch == '\n' || presentation_character_is_safe(ch))
                || recommendation
                    .option_ids
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    != count
                || !recommendation
                    .option_ids
                    .iter()
                    .all(|id| self.options.iter().any(|option| option.id == *id))
            {
                return Err(UserDecisionError::InvalidRequest);
            }
        }
        Ok(())
    }
}

impl super::UserDecisionOption {
    pub(super) fn validate_tradeoffs(&self) -> Result<(), UserDecisionError> {
        for points in [&self.pros, &self.cons] {
            if points.len() > UserDecisionPolicy::OPTION_TRADEOFF_COUNT_MAX
                || points.iter().any(|point| {
                    point.trim().is_empty()
                        || point.len() > UserDecisionPolicy::OPTION_TRADEOFF_MAX_BYTES
                        || !point
                            .chars()
                            .all(|ch| ch == '\n' || presentation_character_is_safe(ch))
                })
            {
                return Err(UserDecisionError::InvalidRequest);
            }
        }
        Ok(())
    }
}

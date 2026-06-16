//! Agent context-window usage: how full each running agent's context window is,
//! and the combined gauge the home screen shows at the bottom of the sidebar.
//!
//! This module is pure domain: value types plus a [`UsageReader`] trait that the
//! infrastructure layer implements per agent CLI (Claude, Gemini, …). Reading a
//! transcript is I/O, so it lives in `infrastructure::agent_usage`; everything
//! here — the per-model context-window sizes, the aggregation, and the display
//! ratios — is computed without touching the filesystem and is tested directly.

use std::path::Path;

/// The default context-window size (tokens) assumed for a model whose id we do
/// not recognise — the standard window for current Claude models.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;

/// The extended context-window size (tokens) for the 1M-context model variants.
pub const EXTENDED_CONTEXT_WINDOW: u64 = 1_000_000;

/// The context-window size (in tokens) for the model an agent is running, keyed
/// off the model id its transcript records.
///
/// The 1M-context variants are detected by their `1m` marker (e.g.
/// `claude-opus-4-8[1m]`); Gemini models carry a 1M window too; everything else
/// falls back to [`DEFAULT_CONTEXT_WINDOW`]. This is a coarse table rather than a
/// live lookup, so a brand-new model simply reads as the default window until
/// it is added here.
pub fn context_window_for(model: &str) -> u64 {
    let model = model.to_ascii_lowercase();
    if model.contains("1m") || model.contains("gemini") {
        EXTENDED_CONTEXT_WINDOW
    } else {
        DEFAULT_CONTEXT_WINDOW
    }
}

/// The context-window usage of a single running agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentUsage {
    /// Tokens currently occupying the model's context window — the most recent
    /// request's input plus its cached (read and freshly written) tokens.
    pub used_tokens: u64,
    /// The model's total context-window size in tokens.
    pub limit_tokens: u64,
}

impl AgentUsage {
    /// A usage reading of `used_tokens` against a `limit_tokens` window.
    pub fn new(used_tokens: u64, limit_tokens: u64) -> Self {
        Self {
            used_tokens,
            limit_tokens,
        }
    }
}

/// The combined context-window usage across every live agent session, shown as a
/// single gauge in the sidebar (the user asked for one aggregate figure, not a
/// per-session breakdown).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AggregateUsage {
    /// Sum of the live sessions' occupied context tokens.
    pub used_tokens: u64,
    /// Sum of the live sessions' context-window sizes.
    pub limit_tokens: u64,
}

impl AggregateUsage {
    /// Fold the live sessions' usages into one aggregate, or `None` when none
    /// reported any — so the sidebar shows nothing rather than an empty `0/0`
    /// gauge when no agent is running (or none has emitted usage yet).
    pub fn from_sessions(usages: impl IntoIterator<Item = AgentUsage>) -> Option<Self> {
        let mut aggregate: Option<AggregateUsage> = None;
        for usage in usages {
            let acc = aggregate.get_or_insert_with(AggregateUsage::default);
            acc.used_tokens = acc.used_tokens.saturating_add(usage.used_tokens);
            acc.limit_tokens = acc.limit_tokens.saturating_add(usage.limit_tokens);
        }
        aggregate
    }

    /// The fraction of the combined window still free (0.0–1.0). Usage above the
    /// limit clamps to 0.0, and a zero limit (no known window) reads as full.
    pub fn remaining_ratio(&self) -> f64 {
        if self.limit_tokens == 0 {
            return 0.0;
        }
        let used = self.used_tokens.min(self.limit_tokens);
        (self.limit_tokens - used) as f64 / self.limit_tokens as f64
    }

    /// The fraction of the combined window already occupied (0.0–1.0), used to
    /// fill the gauge bar.
    pub fn used_ratio(&self) -> f64 {
        1.0 - self.remaining_ratio()
    }

    /// The headroom left to the limit, as a whole percentage for display
    /// ("上限まであと N%").
    pub fn remaining_percent(&self) -> u32 {
        (self.remaining_ratio() * 100.0).round() as u32
    }
}

/// Reads the current context-window usage for the agent running in a worktree.
///
/// Implemented once per agent CLI in `infrastructure::agent_usage`; the home
/// screen's session watcher holds the reader for the configured agent and polls
/// it alongside the bell/liveness checks. `Send` so the watcher thread can own
/// it.
pub trait UsageReader: Send {
    /// The agent's current usage, or `None` when there is no readable transcript
    /// for `worktree` (no agent running, or an agent whose usage we cannot read).
    fn read(&self, worktree: &Path) -> Option<AgentUsage>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_window_recognises_extended_and_gemini_models() {
        assert_eq!(
            context_window_for("claude-opus-4-8[1m]"),
            EXTENDED_CONTEXT_WINDOW
        );
        assert_eq!(
            context_window_for("gemini-2.5-pro"),
            EXTENDED_CONTEXT_WINDOW
        );
        // Ordinary Claude models and unknown ids fall back to the default.
        assert_eq!(
            context_window_for("claude-sonnet-4-6"),
            DEFAULT_CONTEXT_WINDOW
        );
        assert_eq!(context_window_for(""), DEFAULT_CONTEXT_WINDOW);
    }

    #[test]
    fn from_sessions_is_none_when_empty() {
        assert_eq!(AggregateUsage::from_sessions(std::iter::empty()), None);
    }

    #[test]
    fn from_sessions_sums_used_and_limit() {
        let aggregate = AggregateUsage::from_sessions([
            AgentUsage::new(50_000, 200_000),
            AgentUsage::new(100_000, 1_000_000),
        ])
        .expect("two sessions aggregate to Some");
        assert_eq!(aggregate.used_tokens, 150_000);
        assert_eq!(aggregate.limit_tokens, 1_200_000);
    }

    #[test]
    fn remaining_ratio_and_percent_track_headroom() {
        let aggregate = AggregateUsage {
            used_tokens: 50_000,
            limit_tokens: 200_000,
        };
        assert!((aggregate.remaining_ratio() - 0.75).abs() < 1e-9);
        assert!((aggregate.used_ratio() - 0.25).abs() < 1e-9);
        assert_eq!(aggregate.remaining_percent(), 75);
    }

    #[test]
    fn remaining_ratio_clamps_overflow_and_zero_limit() {
        // Used beyond the limit reads as no headroom left, never negative.
        let over = AggregateUsage {
            used_tokens: 300_000,
            limit_tokens: 200_000,
        };
        assert_eq!(over.remaining_ratio(), 0.0);
        assert_eq!(over.remaining_percent(), 0);
        assert!((over.used_ratio() - 1.0).abs() < 1e-9);

        // A zero limit (no known window) reads as full rather than dividing by
        // zero.
        let unknown = AggregateUsage {
            used_tokens: 10,
            limit_tokens: 0,
        };
        assert_eq!(unknown.remaining_ratio(), 0.0);
    }

    #[test]
    fn agent_usage_new_keeps_its_fields() {
        let usage = AgentUsage::new(123, 456);
        assert_eq!(usage.used_tokens, 123);
        assert_eq!(usage.limit_tokens, 456);
    }
}

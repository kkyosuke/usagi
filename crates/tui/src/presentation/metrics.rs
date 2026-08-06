//! Event-driven daemon metrics / git-diff material for the controller Home frame.
//!
//! Daemon metrics (the mascot sidecar) and per-session git diffs (the sidebar
//! column) are pure draw material: the runtime migration design (§4.1) keeps
//! them out of `AppState` because nothing in the reducer reacts to them.  Before
//! this seam the controller frame loop polled the [`MetricsPort`] inline every
//! frame and stashed the result on the legacy `Workspace` view; that coupled the
//! material to a view the strangler migration is deleting.
//!
//! [`MetricsBackend`] instead owns the port and accepts change-only
//! [`MetricsUpdate`] values through a drain the shell consumes into a
//! [`MetricsProjection`] cache — the same one-way discipline
//! (`poll -> drain -> apply -> render`) as
//! [`crate::usecase::application::daemon_backend::DaemonBackend`].  The event
//! type stays in the presentation layer on purpose: [`GitDiff`] is a
//! presentation projection, so routing it through the usecase-layer
//! `controller::BackendEvent` would invert the crate's dependency direction.
//! The design's hint permits exactly this alternative — a drain that returns
//! shell material directly instead of passing through the reducer.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use usagi_core::domain::id::SessionId;
use usagi_core::usecase::client::DaemonMetrics;
use usagi_core::usecase::daemon_health::DaemonHealthTracker;

use crate::presentation::MetricsPort;
use crate::presentation::views::workspace::GitDiff;

/// One changed metrics / git-diff observation returned by [`MetricsBackend`].
///
/// A poll may yield neither, either, or both variants. The shell applies them to
/// a [`MetricsProjection`]; no variant touches the reducer or `AppState`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetricsUpdate {
    /// The latest safe daemon metrics snapshot, or `None` when unavailable.
    Metrics(Option<DaemonMetrics>),
    /// The latest completed per-session git diffs.
    GitDiffs(BTreeMap<SessionId, GitDiff>),
}

/// Shell-owned projection cache for the controller Home frame.
///
/// It is updated only by draining [`MetricsUpdate`]s, keeping the material a
/// pure downstream of the backend poll.  `render_home` reads [`metrics`] and
/// [`git_diffs`] through `HomeProjection::with_metrics` / `with_git_diffs`,
/// which this issue leaves unchanged.
///
/// [`metrics`]: Self::metrics
/// [`git_diffs`]: Self::git_diffs
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MetricsProjection {
    metrics: Option<DaemonMetrics>,
    git_diffs: Arc<BTreeMap<SessionId, GitDiff>>,
    /// Diagnostic-only daemon health, folded from the same drained samples.
    ///
    /// It lives beside the metrics cache because health is a projection *of*
    /// those samples — nothing about it is state the reducer or an operation may
    /// read. Sample deltas need the sample sequence, and this drain is the only
    /// place that sees every observation exactly once.
    health: DaemonHealthTracker,
    generation: u64,
}

impl MetricsProjection {
    /// Fold one drained observation into the cache.
    pub fn apply(&mut self, update: MetricsUpdate) {
        let changed = match update {
            MetricsUpdate::Metrics(metrics) => {
                if self.metrics == metrics {
                    return;
                }
                // An absent observation is not evidence about the daemon: the
                // production port replays its last sample while the lane fails,
                // and a daemon-less workspace never observes at all. Folding
                // only what we actually saw keeps freshness the sole authority
                // on "we stopped hearing from it".
                if let Some(metrics) = metrics.as_ref() {
                    self.health.observe(metrics);
                }
                self.metrics = metrics;
                true
            }
            MetricsUpdate::GitDiffs(git_diffs) => {
                if self.git_diffs.as_ref() == &git_diffs {
                    return;
                }
                self.git_diffs = Arc::new(git_diffs);
                true
            }
        };
        if changed {
            self.generation = self.generation.saturating_add(1);
        }
    }

    /// The diagnostic health observed so far, for the Home frame material.
    #[must_use]
    pub const fn health(&self) -> DaemonHealthTracker {
        self.health
    }

    /// The latest daemon metrics, for `HomeProjection::with_metrics`.
    #[must_use]
    pub fn metrics(&self) -> Option<DaemonMetrics> {
        self.metrics.clone()
    }

    /// The latest per-session git diffs, for `HomeProjection::with_git_diffs`.
    #[must_use]
    pub fn git_diffs(&self) -> &BTreeMap<SessionId, GitDiff> {
        &self.git_diffs
    }

    #[must_use]
    pub fn shared_git_diffs(&self) -> Arc<BTreeMap<SessionId, GitDiff>> {
        Arc::clone(&self.git_diffs)
    }

    /// Monotonic generation of material that actually changed.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

/// Polls a [`MetricsPort`] and exposes changed observations through a drain the
/// controller frame loop consumes, without a same-thread self-channel.
///
/// The port owns the sampling cadence (a one-second metrics cache and a git-diff
/// worker thread in the composition root), so a per-frame [`poll`] preserves the
/// legacy behaviour while the loop stops calling `latest()` / `git_diffs()`
/// directly.  The completion channel mirrors `DaemonBackend`, giving the same
/// `drain_events` seam a fake port drives in tests.
///
/// [`poll`]: Self::poll
pub struct MetricsBackend {
    port: Box<dyn MetricsPort>,
    updates: Vec<MetricsUpdate>,
}

impl MetricsBackend {
    /// Wrap a metrics port behind the reflux drain.
    #[must_use]
    pub fn new(port: Box<dyn MetricsPort>) -> Self {
        Self {
            port,
            updates: Vec::new(),
        }
    }

    /// Sample the port once. Session paths are provided only when their
    /// authoritative projection changed.
    pub fn poll(&mut self, sessions: Option<&[(SessionId, PathBuf)]>) {
        self.updates.extend(self.port.poll_updates(sessions));
    }

    /// Drain every observation refluxed since the last call, without blocking.
    /// The frame loop applies these to its [`MetricsProjection`] at the head of
    /// the frame.
    #[must_use]
    pub fn drain_events(&mut self) -> Vec<MetricsUpdate> {
        std::mem::take(&mut self.updates)
    }
}

#[cfg(test)]
mod tests {
    use super::{MetricsBackend, MetricsProjection, MetricsUpdate};
    use crate::presentation::MetricsPort;
    use crate::presentation::views::workspace::GitDiff;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::rc::Rc;
    use usagi_core::domain::id::SessionId;
    use usagi_core::usecase::client::DaemonMetrics;
    use usagi_core::usecase::daemon_health::DaemonHealthTracker;

    /// A fake port that returns scripted metrics and records the session paths it
    /// was polled with, through a shared handle a test can inspect after boxing.
    struct FakeMetricsPort {
        metrics: Option<DaemonMetrics>,
        git_diffs: BTreeMap<SessionId, GitDiff>,
        polled_sessions: Rc<RefCell<Vec<(SessionId, PathBuf)>>>,
        emitted: bool,
    }

    impl MetricsPort for FakeMetricsPort {
        fn poll_updates(
            &mut self,
            sessions: Option<&[(SessionId, PathBuf)]>,
        ) -> Vec<MetricsUpdate> {
            if let Some(sessions) = sessions {
                *self.polled_sessions.borrow_mut() = sessions.to_vec();
            }
            if std::mem::replace(&mut self.emitted, true) {
                Vec::new()
            } else {
                vec![
                    MetricsUpdate::Metrics(self.metrics.clone()),
                    MetricsUpdate::GitDiffs(self.git_diffs.clone()),
                ]
            }
        }
    }

    fn git_diff() -> GitDiff {
        GitDiff {
            base: "main".to_owned(),
            ahead: 2,
            behind: 1,
            added: 4,
            removed: 3,
        }
    }

    fn metrics() -> DaemonMetrics {
        DaemonMetrics {
            schema_version: 1,
            sampled_at_ms: 42,
            cpu_percent_hundredths: 250,
            resident_memory_bytes: 45 * 1024 * 1024,
            active_subscribers: 3,
            dropped_updates: 0,
            terminal_dropped_bytes: 0,
            terminal_coalesced_bytes: 0,
            terminal_backpressured_bytes: 0,
            pr_projection_dropped_bytes: 0,
            pr_projection_coalesced_bytes: 0,
            pr_projection_gaps: 0,
            agent_concurrency: None,
            failed_background_workers: 0,
        }
    }

    #[test]
    fn poll_refluxes_metrics_then_git_diffs_in_order() {
        let session = SessionId::new();
        let mut diffs = BTreeMap::new();
        diffs.insert(session, git_diff());
        let mut backend = MetricsBackend::new(Box::new(FakeMetricsPort {
            metrics: Some(metrics()),
            git_diffs: diffs,
            polled_sessions: Rc::new(RefCell::new(Vec::new())),
            emitted: false,
        }));

        backend.poll(Some(&[(session, PathBuf::from("/work/alpha"))]));
        let events = backend.drain_events();
        assert!(matches!(
            events.as_slice(),
            [
                MetricsUpdate::Metrics(Some(m)),
                MetricsUpdate::GitDiffs(g),
            ] if *m == metrics() && g.get(&session) == Some(&git_diff())
        ));
        // The drain empties the channel.
        assert!(backend.drain_events().is_empty());
    }

    #[test]
    fn poll_forwards_the_session_paths_and_refluxes_absent_metrics() {
        let session = SessionId::new();
        let polled = Rc::new(RefCell::new(Vec::new()));
        let mut backend = MetricsBackend::new(Box::new(FakeMetricsPort {
            metrics: None,
            git_diffs: BTreeMap::new(),
            polled_sessions: Rc::clone(&polled),
            emitted: false,
        }));

        backend.poll(Some(&[(session, PathBuf::from("/work/beta"))]));
        assert!(matches!(
            backend.drain_events().as_slice(),
            [MetricsUpdate::Metrics(None), MetricsUpdate::GitDiffs(g)] if g.is_empty()
        ));
        // The session list reached the git-diff worker unchanged.
        assert_eq!(
            polled.borrow().as_slice(),
            [(session, PathBuf::from("/work/beta"))]
        );
    }

    #[test]
    fn projection_applies_the_last_observation_of_each_kind() {
        let session = SessionId::new();
        let mut projection = MetricsProjection::default();
        assert_eq!(projection.metrics(), None);
        assert!(projection.git_diffs().is_empty());

        projection.apply(MetricsUpdate::Metrics(Some(metrics())));
        let mut diffs = BTreeMap::new();
        diffs.insert(session, git_diff());
        projection.apply(MetricsUpdate::GitDiffs(diffs));
        assert_eq!(projection.metrics(), Some(metrics()));
        assert_eq!(projection.git_diffs().get(&session), Some(&git_diff()));

        // A later poll with the daemon gone clears metrics; the last git diffs win.
        projection.apply(MetricsUpdate::Metrics(None));
        projection.apply(MetricsUpdate::GitDiffs(BTreeMap::new()));
        assert_eq!(projection.metrics(), None);
        assert!(projection.git_diffs().is_empty());
    }

    #[test]
    fn drained_updates_fold_into_the_projection() {
        let session = SessionId::new();
        let mut diffs = BTreeMap::new();
        diffs.insert(session, git_diff());
        let mut backend = MetricsBackend::new(Box::new(FakeMetricsPort {
            metrics: Some(metrics()),
            git_diffs: diffs,
            polled_sessions: Rc::new(RefCell::new(Vec::new())),
            emitted: false,
        }));
        let mut projection = MetricsProjection::default();

        backend.poll(Some(&[(session, PathBuf::from("/work/alpha"))]));
        for update in backend.drain_events() {
            projection.apply(update);
        }
        assert_eq!(projection.metrics(), Some(metrics()));
        assert_eq!(projection.git_diffs().get(&session), Some(&git_diff()));
    }

    #[test]
    fn drained_samples_advance_the_diagnostic_health_tracker() {
        let mut projection = MetricsProjection::default();
        // 観測前は既定値（indicator を出さない状態）。
        assert_eq!(projection.health(), DaemonHealthTracker::default());

        let mut first = metrics();
        first.sampled_at_ms = 1_000;
        projection.apply(MetricsUpdate::Metrics(Some(first.clone())));
        let mut expected = DaemonHealthTracker::default();
        expected.observe(&first);
        assert_eq!(projection.health(), expected);

        // 観測できなかった poll は tracker を巻き戻さない。metrics cache だけが空になる。
        projection.apply(MetricsUpdate::Metrics(None));
        assert_eq!(projection.metrics(), None);
        assert_eq!(projection.health(), expected);

        // git diff だけの update も health を動かさない。
        projection.apply(MetricsUpdate::GitDiffs(BTreeMap::new()));
        assert_eq!(projection.health(), expected);
    }

    #[test]
    fn one_changed_git_map_is_moved_once_across_one_thousand_idle_polls() {
        let session = SessionId::new();
        let mut backend = MetricsBackend::new(Box::new(FakeMetricsPort {
            metrics: None,
            git_diffs: BTreeMap::from([(session, git_diff())]),
            polled_sessions: Rc::new(RefCell::new(Vec::new())),
            emitted: false,
        }));
        let mut projection = MetricsProjection::default();
        let paths = [(session, PathBuf::from("/work/alpha"))];

        for tick in 0..1_000 {
            backend.poll((tick == 0).then_some(paths.as_slice()));
            for update in backend.drain_events() {
                projection.apply(update);
            }
        }

        assert_eq!(projection.generation(), 1);
        assert_eq!(projection.git_diffs().get(&session), Some(&git_diff()));
    }

    #[test]
    fn metrics_update_derives_round_trip() {
        let update = MetricsUpdate::Metrics(Some(metrics()));
        assert_eq!(update.clone(), update);
        assert!(format!("{update:?}").contains("Metrics"));
    }
}

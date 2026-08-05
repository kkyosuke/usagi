//! Bounded fan-out and canonical snapshots for daemon metrics. A slow observer
//! can lose intermediate samples, but it can never delay the daemon or another
//! observer.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};

use usagi_core::usecase::client::{AgentConcurrency, DaemonMetrics};

use super::shutdown::BackgroundWorkerHealth;

/// The Agent concurrency level, published by the authority that admits Agent
/// launches and read by the metrics broker.
///
/// It exists because the two sides must not share a lock. The admission
/// authority is the Agent runtime, whose mutex is held across a PTY spawn; a
/// metrics tick that waited for it would make a display-only observation delay
/// the daemon, which is exactly what the metrics contract forbids. So the
/// authority *pushes* its own `occupied_slots` / limit here after every durable
/// mutation, and the broker reads it without blocking. No number is duplicated:
/// both come from the coordinator's own accessors.
///
/// A gauge nobody has published to reads as unknown rather than as `0 / 0`, so a
/// composition that never bound one cannot be mistaken for an idle daemon.
#[derive(Debug, Clone, Default)]
pub struct AgentConcurrencyGauge(Arc<GaugeCell>);

/// The pair travels in one `u64` so a reader can never see an `in_use` from one
/// publication beside a `limit` from another.
#[derive(Debug, Default)]
struct GaugeCell {
    published: AtomicBool,
    pair: AtomicU64,
}

impl AgentConcurrencyGauge {
    /// Records the authority's current level. Counts above `u32::MAX` saturate,
    /// which cannot happen for a bounded pool but keeps the encoding total.
    pub fn publish(&self, in_use: usize, limit: usize) {
        let in_use = u32::try_from(in_use).unwrap_or(u32::MAX);
        let limit = u32::try_from(limit).unwrap_or(u32::MAX);
        self.0.pair.store(
            (u64::from(limit) << 32) | u64::from(in_use),
            Ordering::Release,
        );
        self.0.published.store(true, Ordering::Release);
    }

    /// The last published level, or `None` while no authority has published one.
    ///
    /// This never blocks, so a metrics tick observes it while the Agent runtime
    /// is busy spawning. Reading a level one publication stale is acceptable for
    /// a display-only projection; reporting a torn pair would not be.
    #[must_use]
    pub fn observe(&self) -> Option<AgentConcurrency> {
        if !self.0.published.load(Ordering::Acquire) {
            return None;
        }
        let pair = self.0.pair.load(Ordering::Acquire);
        Some(AgentConcurrency {
            in_use: u32::try_from(pair & u64::from(u32::MAX)).unwrap_or(u32::MAX),
            limit: u32::try_from(pair >> 32).unwrap_or(u32::MAX),
        })
    }
}

/// A daemon-local subscription token. It is intentionally not a durable
/// resource identity: reconnecting creates a fresh observer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MetricsSubscription(pub u64);

/// Raw process-local observations supplied by the composition root.
///
/// Subscriber and backpressure fields are deliberately absent: the broker is
/// their only authority and adds them when it builds the wire snapshot.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MetricsSample {
    pub sampled_at_ms: u64,
    pub cpu_percent_hundredths: u32,
    pub resident_memory_bytes: u64,
    pub terminal_dropped_bytes: u64,
    pub terminal_coalesced_bytes: u64,
    pub terminal_backpressured_bytes: u64,
    pub pr_projection_dropped_bytes: u64,
    pub pr_projection_coalesced_bytes: u64,
    pub pr_projection_gaps: u64,
}

/// The receiving side of one bounded metrics subscription.
#[derive(Debug)]
pub struct MetricsObserver {
    subscription: MetricsSubscription,
    receiver: Receiver<DaemonMetrics>,
}

impl MetricsObserver {
    #[must_use]
    pub const fn subscription(&self) -> MetricsSubscription {
        self.subscription
    }

    /// Reads the next queued snapshot without blocking.
    ///
    /// # Errors
    ///
    /// Returns `Empty` when no tick is queued and `Disconnected` after the
    /// broker removes this observer.
    pub fn try_recv(&self) -> Result<DaemonMetrics, TryRecvError> {
        self.receiver.try_recv()
    }
}

/// Fan-out broker with one coalescible slot per client and one canonical
/// process-local snapshot.
#[derive(Debug, Default)]
pub struct MetricsBroker {
    next: u64,
    subscribers: BTreeMap<MetricsSubscription, SyncSender<DaemonMetrics>>,
    dropped_updates: u64,
    latest: MetricsSample,
    agent_concurrency: AgentConcurrencyGauge,
    background_workers: BackgroundWorkerHealth,
}

impl MetricsBroker {
    /// Builds a broker that reports the Agent concurrency this gauge carries.
    ///
    /// Concurrency is not part of [`MetricsSample`] for the same reason the
    /// subscriber and drop counters are not: the broker is where a wire snapshot
    /// is assembled from the authorities that own each number. The gauge is read
    /// on every snapshot, so `subscribe` / `unsubscribe` replies carry the same
    /// live level a published tick does.
    #[must_use]
    pub fn with_agent_concurrency(agent_concurrency: AgentConcurrencyGauge) -> Self {
        Self {
            agent_concurrency,
            ..Self::default()
        }
    }

    /// Binds both runtime authorities read by the wire snapshot without locks.
    #[must_use]
    pub fn with_runtime_health(
        agent_concurrency: AgentConcurrencyGauge,
        background_workers: BackgroundWorkerHealth,
    ) -> Self {
        Self {
            agent_concurrency,
            background_workers,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn subscribe(&mut self) -> MetricsObserver {
        self.next = self.next.saturating_add(1);
        let subscription = MetricsSubscription(self.next);
        let (sender, receiver) = sync_channel(1);
        self.subscribers.insert(subscription, sender);
        MetricsObserver {
            subscription,
            receiver,
        }
    }

    pub fn unsubscribe(&mut self, subscription: MetricsSubscription) -> bool {
        self.subscribers.remove(&subscription).is_some()
    }

    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.subscribers.len()
    }

    /// Returns the latest raw observation decorated with broker-owned counters.
    #[must_use]
    pub fn snapshot(&self) -> DaemonMetrics {
        DaemonMetrics {
            schema_version: 4,
            sampled_at_ms: self.latest.sampled_at_ms,
            cpu_percent_hundredths: self.latest.cpu_percent_hundredths,
            resident_memory_bytes: self.latest.resident_memory_bytes,
            active_subscribers: u32::try_from(self.subscribers.len()).unwrap_or(u32::MAX),
            dropped_updates: self.dropped_updates,
            terminal_dropped_bytes: self.latest.terminal_dropped_bytes,
            terminal_coalesced_bytes: self.latest.terminal_coalesced_bytes,
            terminal_backpressured_bytes: self.latest.terminal_backpressured_bytes,
            pr_projection_dropped_bytes: self.latest.pr_projection_dropped_bytes,
            pr_projection_coalesced_bytes: self.latest.pr_projection_coalesced_bytes,
            pr_projection_gaps: self.latest.pr_projection_gaps,
            agent_concurrency: self.agent_concurrency.observe(),
            failed_background_workers: self.background_workers.failed_count(),
        }
    }

    /// Publishes one snapshot without blocking and returns the canonical state
    /// after drop accounting and disconnected-observer cleanup.
    pub fn publish(&mut self, sample: MetricsSample) -> DaemonMetrics {
        self.latest = sample;
        let queued = self.snapshot();
        self.subscribers
            .retain(|_, sender| match sender.try_send(queued.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    self.dropped_updates = self.dropped_updates.saturating_add(1);
                    true
                }
                Err(TrySendError::Disconnected(_)) => false,
            });
        self.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(sampled_at_ms: u64) -> MetricsSample {
        MetricsSample {
            sampled_at_ms,
            cpu_percent_hundredths: 125,
            resident_memory_bytes: 4096,
            terminal_dropped_bytes: 3,
            terminal_coalesced_bytes: 5,
            terminal_backpressured_bytes: 7,
            pr_projection_dropped_bytes: 0,
            pr_projection_coalesced_bytes: 0,
            pr_projection_gaps: 0,
        }
    }

    #[test]
    fn fans_out_canonical_snapshots_to_multiple_clients() {
        let mut broker = MetricsBroker::default();
        let first = broker.subscribe();
        let second = broker.subscribe();
        let snapshot = broker.publish(sample(42));
        assert_eq!(first.try_recv().unwrap(), snapshot);
        assert_eq!(second.try_recv().unwrap().active_subscribers, 2);
        assert_eq!(snapshot.cpu_percent_hundredths, 125);
        assert_eq!(snapshot.terminal_backpressured_bytes, 7);
    }

    #[test]
    fn unregister_stops_only_the_selected_client() {
        let mut broker = MetricsBroker::default();
        let first = broker.subscribe();
        let second = broker.subscribe();
        assert!(broker.unsubscribe(first.subscription()));
        let snapshot = broker.publish(sample(7));
        assert_eq!(first.try_recv(), Err(TryRecvError::Disconnected));
        assert_eq!(second.try_recv().unwrap().sampled_at_ms, 7);
        assert_eq!(snapshot.active_subscribers, 1);
    }

    #[test]
    fn slow_client_is_bounded_and_does_not_block_other_clients() {
        let mut broker = MetricsBroker::default();
        let slow = broker.subscribe();
        let fast = broker.subscribe();
        broker.publish(sample(1));
        assert_eq!(fast.try_recv().unwrap().sampled_at_ms, 1);
        let snapshot = broker.publish(sample(2));
        assert_eq!(slow.try_recv().unwrap().sampled_at_ms, 1);
        assert_eq!(fast.try_recv().unwrap().sampled_at_ms, 2);
        assert_eq!(snapshot.dropped_updates, 1);
        assert_eq!(broker.snapshot(), snapshot);
    }

    #[test]
    fn disconnected_client_is_removed_on_the_next_tick() {
        let mut broker = MetricsBroker::default();
        let observer = broker.subscribe();
        drop(observer);
        let snapshot = broker.publish(sample(1));
        assert_eq!(broker.subscriber_count(), 0);
        assert_eq!(snapshot.active_subscribers, 0);
    }

    #[test]
    fn an_unbound_broker_reports_agent_concurrency_as_unknown() {
        let mut broker = MetricsBroker::default();
        assert_eq!(broker.snapshot().agent_concurrency, None);
        assert_eq!(broker.publish(sample(1)).agent_concurrency, None);
        // Schema 4 carries both runtime health projections.
        assert_eq!(broker.snapshot().schema_version, 4);
    }

    #[test]
    fn a_bound_gauge_reports_the_authority_level_on_every_snapshot() {
        let gauge = AgentConcurrencyGauge::default();
        let mut broker = MetricsBroker::with_agent_concurrency(gauge.clone());
        // Bound but not yet published: still unknown, not an implied zero.
        assert_eq!(broker.snapshot().agent_concurrency, None);

        gauge.publish(3, 16);
        let observer = broker.subscribe();
        assert_eq!(
            broker.snapshot().agent_concurrency,
            Some(AgentConcurrency {
                in_use: 3,
                limit: 16
            })
        );
        // A published tick fans out the same level the snapshot reports.
        assert_eq!(
            broker.publish(sample(2)).agent_concurrency,
            Some(AgentConcurrency {
                in_use: 3,
                limit: 16
            })
        );
        assert_eq!(
            observer.try_recv().unwrap().agent_concurrency,
            Some(AgentConcurrency {
                in_use: 3,
                limit: 16
            })
        );

        // A later publication is picked up without another sample: the level is
        // read from the authority, not cached beside the process counters.
        gauge.publish(16, 16);
        assert_eq!(
            broker.snapshot().agent_concurrency,
            Some(AgentConcurrency {
                in_use: 16,
                limit: 16
            })
        );
    }

    #[test]
    fn a_gauge_clone_is_the_same_authority_and_saturates_at_the_encoding_bound() {
        let gauge = AgentConcurrencyGauge::default();
        let reader = gauge.clone();
        gauge.publish(1, 16);
        assert_eq!(
            reader.observe(),
            Some(AgentConcurrency {
                in_use: 1,
                limit: 16
            })
        );

        // A count wider than the wire field saturates instead of wrapping into a
        // smaller, plausible-looking number.
        gauge.publish(usize::MAX, usize::MAX);
        assert_eq!(
            reader.observe(),
            Some(AgentConcurrency {
                in_use: u32::MAX,
                limit: u32::MAX
            })
        );
    }

    #[test]
    fn a_new_broker_starts_a_fresh_process_incarnation() {
        let mut previous = MetricsBroker::default();
        let _slow = previous.subscribe();
        previous.publish(sample(1));
        assert_eq!(previous.publish(sample(2)).dropped_updates, 1);

        let restarted = MetricsBroker::default();
        assert_eq!(restarted.snapshot().active_subscribers, 0);
        assert_eq!(restarted.snapshot().dropped_updates, 0);
    }
}

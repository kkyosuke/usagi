//! Bounded fan-out and canonical snapshots for daemon metrics. A slow observer
//! can lose intermediate samples, but it can never delay the daemon or another
//! observer.

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};

use usagi_core::usecase::client::DaemonMetrics;

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
}

impl MetricsBroker {
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
            schema_version: 2,
            sampled_at_ms: self.latest.sampled_at_ms,
            cpu_percent_hundredths: self.latest.cpu_percent_hundredths,
            resident_memory_bytes: self.latest.resident_memory_bytes,
            active_subscribers: u32::try_from(self.subscribers.len()).unwrap_or(u32::MAX),
            dropped_updates: self.dropped_updates,
            terminal_dropped_bytes: self.latest.terminal_dropped_bytes,
            terminal_coalesced_bytes: self.latest.terminal_coalesced_bytes,
            terminal_backpressured_bytes: self.latest.terminal_backpressured_bytes,
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

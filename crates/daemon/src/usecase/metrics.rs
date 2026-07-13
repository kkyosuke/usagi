//! Bounded fan-out for daemon metrics.  A slow observer can lose intermediate
//! samples, but it can never delay the daemon or another observer.

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};

use usagi_core::usecase::client::DaemonMetrics;

/// A daemon-local subscription token.  It is intentionally not a durable
/// resource identity: reconnecting creates a fresh observer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MetricsSubscription(pub u64);

#[derive(Debug)]
struct Subscriber {
    sender: SyncSender<DaemonMetrics>,
    dropped_updates: u64,
}

/// Fan-out broker with one coalescible slot per client.
#[derive(Debug, Default)]
pub struct MetricsBroker {
    next: u64,
    subscribers: BTreeMap<MetricsSubscription, Subscriber>,
    dropped_updates: u64,
}

impl MetricsBroker {
    #[must_use]
    pub fn subscribe(&mut self) -> (MetricsSubscription, Receiver<DaemonMetrics>) {
        self.next += 1;
        let subscription = MetricsSubscription(self.next);
        let (sender, receiver) = sync_channel(1);
        self.subscribers.insert(
            subscription,
            Subscriber {
                sender,
                dropped_updates: 0,
            },
        );
        (subscription, receiver)
    }

    pub fn unsubscribe(&mut self, subscription: MetricsSubscription) -> bool {
        self.subscribers.remove(&subscription).is_some()
    }

    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.subscribers.len()
    }

    /// Publishes one snapshot without blocking.  A full client slot keeps its
    /// newest already queued snapshot and accounts a dropped intermediate
    /// sample; a disconnected client is removed immediately.
    pub fn publish(&mut self, sampled_at_ms: u64) {
        let snapshot = DaemonMetrics {
            schema_version: 1,
            sampled_at_ms,
            active_subscribers: u32::try_from(self.subscribers.len()).unwrap_or(u32::MAX),
            dropped_updates: self.dropped_updates,
        };
        self.subscribers.retain(|_, subscriber| {
            match subscriber.sender.try_send(snapshot.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    subscriber.dropped_updates += 1;
                    self.dropped_updates += 1;
                    true
                }
                Err(TrySendError::Disconnected(_)) => false,
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::TryRecvError;

    #[test]
    fn fans_out_periodic_snapshots_to_multiple_clients() {
        let mut broker = MetricsBroker::default();
        let (_, first) = broker.subscribe();
        let (_, second) = broker.subscribe();
        broker.publish(42);
        assert_eq!(first.recv().unwrap().sampled_at_ms, 42);
        assert_eq!(second.recv().unwrap().active_subscribers, 2);
    }

    #[test]
    fn unregister_stops_only_the_selected_client() {
        let mut broker = MetricsBroker::default();
        let (first_id, first) = broker.subscribe();
        let (_, second) = broker.subscribe();
        assert!(broker.unsubscribe(first_id));
        broker.publish(7);
        assert_eq!(first.try_recv(), Err(TryRecvError::Disconnected));
        assert_eq!(second.recv().unwrap().sampled_at_ms, 7);
    }

    #[test]
    fn slow_client_is_bounded_and_does_not_block_other_clients() {
        let mut broker = MetricsBroker::default();
        let (_, slow) = broker.subscribe();
        let (_, fast) = broker.subscribe();
        broker.publish(1);
        broker.publish(2);
        assert_eq!(slow.recv().unwrap().sampled_at_ms, 1);
        assert_eq!(fast.recv().unwrap().sampled_at_ms, 1);
        assert_eq!(broker.dropped_updates, 2);
    }

    #[test]
    fn disconnected_client_is_removed_on_the_next_tick() {
        let mut broker = MetricsBroker::default();
        let (_, receiver) = broker.subscribe();
        drop(receiver);
        broker.publish(1);
        assert_eq!(broker.subscriber_count(), 0);
    }
}

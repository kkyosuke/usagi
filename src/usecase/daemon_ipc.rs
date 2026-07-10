//! The daemon side of the IPC protocol: which connected clients want session
//! pushes, and how each incoming [`ClientMessage`] is answered.
//!
//! This is the pure bookkeeping the socket server drives. The composition root
//! owns the sockets and the per-client threads; it hands each decoded message
//! here with the connection's id and the current snapshot, applies the returned
//! reply, and consults [`SubscriberRegistry::subscribers`] when a snapshot change
//! must be pushed. Keeping the registry and the dispatch free of IO makes every
//! branch unit-testable without a socket.

use std::collections::HashSet;

use crate::domain::daemon::SessionSnapshot;
use crate::domain::daemon_ipc::{ClientMessage, ServerMessage};

/// Identifies one connected client for the life of its connection. Assigned by
/// the socket server as connections are accepted.
pub type ClientId = u64;

/// The set of clients currently subscribed to session-snapshot pushes.
#[derive(Debug, Default)]
pub struct SubscriberRegistry {
    subscribers: HashSet<ClientId>,
}

impl SubscriberRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Start pushing snapshots to `client`.
    pub fn subscribe(&mut self, client: ClientId) {
        self.subscribers.insert(client);
    }

    /// Stop pushing snapshots to `client` — on an explicit `Unsubscribe` or when
    /// the connection drops. Idempotent.
    pub fn remove(&mut self, client: ClientId) {
        self.subscribers.remove(&client);
    }

    /// Whether `client` currently receives pushes.
    pub fn is_subscribed(&self, client: ClientId) -> bool {
        self.subscribers.contains(&client)
    }

    /// The clients a snapshot change must be pushed to.
    pub fn subscribers(&self) -> Vec<ClientId> {
        let mut ids: Vec<ClientId> = self.subscribers.iter().copied().collect();
        ids.sort_unstable();
        ids
    }
}

/// Answer one `message` from `client`, updating `registry` and returning the
/// immediate reply to send back (or `None` when the message needs no reply).
/// `sessions` is the daemon's current monitored-sessions snapshot.
pub fn handle(
    message: ClientMessage,
    client: ClientId,
    registry: &mut SubscriberRegistry,
    sessions: &[SessionSnapshot],
) -> Option<ServerMessage> {
    match message {
        ClientMessage::ListSessions => Some(ServerMessage::Sessions {
            sessions: sessions.to_vec(),
        }),
        ClientMessage::Subscribe => {
            registry.subscribe(client);
            Some(ServerMessage::Sessions {
                sessions: sessions.to_vec(),
            })
        }
        ClientMessage::Unsubscribe => {
            registry.remove(client);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::daemon::SessionActivity;
    use std::path::PathBuf;

    fn sample_sessions() -> Vec<SessionSnapshot> {
        vec![SessionSnapshot {
            workspace: PathBuf::from("/repo"),
            name: "work".to_string(),
            activity: Some(SessionActivity::Running),
        }]
    }

    #[test]
    fn registry_tracks_subscribe_and_remove() {
        let mut registry = SubscriberRegistry::new();
        assert!(!registry.is_subscribed(1));
        registry.subscribe(1);
        registry.subscribe(2);
        assert!(registry.is_subscribed(1));
        assert_eq!(registry.subscribers(), vec![1, 2]);
        registry.remove(1);
        assert!(!registry.is_subscribed(1));
        assert_eq!(registry.subscribers(), vec![2]);
        // Removing an absent client is a no-op.
        registry.remove(1);
        assert_eq!(registry.subscribers(), vec![2]);
    }

    #[test]
    fn list_sessions_replies_with_the_snapshot_without_subscribing() {
        let mut registry = SubscriberRegistry::new();
        let reply = handle(
            ClientMessage::ListSessions,
            7,
            &mut registry,
            &sample_sessions(),
        );
        assert_eq!(
            reply,
            Some(ServerMessage::Sessions {
                sessions: sample_sessions()
            })
        );
        assert!(!registry.is_subscribed(7));
    }

    #[test]
    fn subscribe_registers_and_replies_with_the_current_snapshot() {
        let mut registry = SubscriberRegistry::new();
        let reply = handle(
            ClientMessage::Subscribe,
            7,
            &mut registry,
            &sample_sessions(),
        );
        assert_eq!(
            reply,
            Some(ServerMessage::Sessions {
                sessions: sample_sessions()
            })
        );
        assert!(registry.is_subscribed(7));
    }

    #[test]
    fn unsubscribe_removes_and_has_no_reply() {
        let mut registry = SubscriberRegistry::new();
        registry.subscribe(7);
        let reply = handle(
            ClientMessage::Unsubscribe,
            7,
            &mut registry,
            &sample_sessions(),
        );
        assert_eq!(reply, None);
        assert!(!registry.is_subscribed(7));
    }
}

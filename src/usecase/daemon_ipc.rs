//! The daemon side of the IPC protocol: which connected clients want session
//! pushes, and how each incoming [`ClientMessage`] is answered.
//!
//! This is the pure bookkeeping the socket server drives. The composition root
//! owns the sockets and the per-client threads; it hands each decoded message
//! here with the connection's id and the current snapshot, applies the returned
//! reply, and consults [`SubscriberRegistry::subscribers`] when a snapshot change
//! must be pushed. Keeping the registry and the dispatch free of IO makes every
//! branch unit-testable without a socket.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::domain::daemon::SessionSnapshot;
use crate::domain::daemon_ipc::{ClientMessage, ServerMessage};

/// Identifies one connected client for the life of its connection. Assigned by
/// the socket server as connections are accepted.
pub type ClientId = u64;

/// What the socket server should do in response to a message, decided purely by
/// [`handle`]. Replies are sent as-is; the terminal actions carry real PTY IO the
/// composition root performs (spawning / killing the daemon-owned process), which
/// is why they are returned rather than executed here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Send this message back to the requesting client.
    Reply(ServerMessage),
    /// Spawn (or reuse) the daemon-owned terminal for this worktree, then reply.
    Spawn(PathBuf),
    /// Kill the daemon-owned terminal for this worktree, then reply.
    Kill(PathBuf),
    /// Attach the requesting client to this worktree's screen feed, then send its
    /// current screen.
    Attach(PathBuf),
    /// Detach the requesting client from this worktree's screen feed.
    Detach(PathBuf),
    /// Write these input bytes to this worktree's terminal.
    Keys(PathBuf, Vec<u8>),
    /// Resize this worktree's terminal to `cols`×`rows`.
    Resize(PathBuf, u16, u16),
    /// Nothing to send.
    Nothing,
}

/// The daemon-owned terminals, tracked by the worktree they run in and the pid of
/// the process. Pure bookkeeping: the real PTY handles live in the composition
/// root, which mirrors its spawns and kills into this registry so the running
/// set (and the "is one already running here?" decision) stays unit-testable.
#[derive(Debug, Default)]
pub struct TerminalRegistry {
    terminals: HashMap<PathBuf, u32>,
}

impl TerminalRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that a terminal with `pid` runs for `worktree`, replacing any
    /// previous entry for it.
    pub fn insert(&mut self, worktree: PathBuf, pid: u32) {
        self.terminals.insert(worktree, pid);
    }

    /// Forget the terminal for `worktree`, returning its pid if one was tracked.
    pub fn remove(&mut self, worktree: &Path) -> Option<u32> {
        self.terminals.remove(worktree)
    }

    /// Whether a terminal is tracked for `worktree`.
    pub fn contains(&self, worktree: &Path) -> bool {
        self.terminals.contains_key(worktree)
    }

    /// The pid of the terminal running for `worktree`, if any.
    pub fn pid(&self, worktree: &Path) -> Option<u32> {
        self.terminals.get(worktree).copied()
    }

    /// The worktrees with a running terminal, sorted for a stable report.
    pub fn worktrees(&self) -> Vec<PathBuf> {
        let mut worktrees: Vec<PathBuf> = self.terminals.keys().cloned().collect();
        worktrees.sort();
        worktrees
    }
}

/// Which clients are attached to which worktree's screen feed. A client may be
/// attached to several worktrees, and several clients may share one. Pure
/// bookkeeping the socket server consults when a terminal's screen changes.
#[derive(Debug, Default)]
pub struct AttachTable {
    by_worktree: HashMap<PathBuf, HashSet<ClientId>>,
}

impl AttachTable {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach `client` to `worktree`'s screen feed.
    pub fn attach(&mut self, client: ClientId, worktree: PathBuf) {
        self.by_worktree.entry(worktree).or_default().insert(client);
    }

    /// Detach `client` from `worktree`, forgetting the worktree entirely once no
    /// client is attached to it.
    pub fn detach(&mut self, client: ClientId, worktree: &Path) {
        if let Some(clients) = self.by_worktree.get_mut(worktree) {
            clients.remove(&client);
            if clients.is_empty() {
                self.by_worktree.remove(worktree);
            }
        }
    }

    /// Remove `client` from every worktree — used when its connection drops.
    pub fn remove_client(&mut self, client: ClientId) {
        self.by_worktree.retain(|_, clients| {
            clients.remove(&client);
            !clients.is_empty()
        });
    }

    /// The clients attached to `worktree`, sorted for a stable push order.
    pub fn clients_for(&self, worktree: &Path) -> Vec<ClientId> {
        let mut clients: Vec<ClientId> = self
            .by_worktree
            .get(worktree)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default();
        clients.sort_unstable();
        clients
    }

    /// Whether `client` is attached to `worktree`.
    pub fn is_attached(&self, client: ClientId, worktree: &Path) -> bool {
        self.by_worktree
            .get(worktree)
            .is_some_and(|clients| clients.contains(&client))
    }
}

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

/// Decide what to do with one `message` from `client`, updating the subscriber
/// `registry` for the subscription messages. `sessions` is the daemon's current
/// monitored-sessions snapshot. The terminal messages return an [`Action`] the
/// caller performs (they need real PTY IO); the rest resolve to a reply here.
pub fn handle(
    message: ClientMessage,
    client: ClientId,
    registry: &mut SubscriberRegistry,
    sessions: &[SessionSnapshot],
) -> Action {
    match message {
        ClientMessage::ListSessions => Action::Reply(ServerMessage::Sessions {
            sessions: sessions.to_vec(),
        }),
        ClientMessage::Subscribe => {
            registry.subscribe(client);
            Action::Reply(ServerMessage::Sessions {
                sessions: sessions.to_vec(),
            })
        }
        ClientMessage::Unsubscribe => {
            registry.remove(client);
            Action::Nothing
        }
        ClientMessage::Spawn { worktree } => Action::Spawn(worktree),
        ClientMessage::Kill { worktree } => Action::Kill(worktree),
        ClientMessage::Attach { worktree } => Action::Attach(worktree),
        ClientMessage::Detach { worktree } => Action::Detach(worktree),
        ClientMessage::Keys { worktree, data } => Action::Keys(worktree, data),
        ClientMessage::Resize {
            worktree,
            cols,
            rows,
        } => Action::Resize(worktree, cols, rows),
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
        let action = handle(
            ClientMessage::ListSessions,
            7,
            &mut registry,
            &sample_sessions(),
        );
        assert_eq!(
            action,
            Action::Reply(ServerMessage::Sessions {
                sessions: sample_sessions()
            })
        );
        assert!(!registry.is_subscribed(7));
    }

    #[test]
    fn subscribe_registers_and_replies_with_the_current_snapshot() {
        let mut registry = SubscriberRegistry::new();
        let action = handle(
            ClientMessage::Subscribe,
            7,
            &mut registry,
            &sample_sessions(),
        );
        assert_eq!(
            action,
            Action::Reply(ServerMessage::Sessions {
                sessions: sample_sessions()
            })
        );
        assert!(registry.is_subscribed(7));
    }

    #[test]
    fn unsubscribe_removes_and_does_nothing_else() {
        let mut registry = SubscriberRegistry::new();
        registry.subscribe(7);
        let action = handle(
            ClientMessage::Unsubscribe,
            7,
            &mut registry,
            &sample_sessions(),
        );
        assert_eq!(action, Action::Nothing);
        assert!(!registry.is_subscribed(7));
    }

    #[test]
    fn spawn_and_kill_return_terminal_actions() {
        let mut registry = SubscriberRegistry::new();
        let worktree = PathBuf::from("/repo/.usagi/sessions/work");
        assert_eq!(
            handle(
                ClientMessage::Spawn {
                    worktree: worktree.clone()
                },
                1,
                &mut registry,
                &[]
            ),
            Action::Spawn(worktree.clone())
        );
        assert_eq!(
            handle(
                ClientMessage::Kill {
                    worktree: worktree.clone()
                },
                1,
                &mut registry,
                &[]
            ),
            Action::Kill(worktree)
        );
    }

    #[test]
    fn terminal_registry_tracks_insert_pid_and_remove() {
        let mut registry = TerminalRegistry::new();
        let a = PathBuf::from("/a");
        let b = PathBuf::from("/b");
        assert!(!registry.contains(&a));
        assert_eq!(registry.pid(&a), None);
        registry.insert(a.clone(), 111);
        registry.insert(b.clone(), 222);
        assert!(registry.contains(&a));
        assert_eq!(registry.pid(&a), Some(111));
        assert_eq!(registry.worktrees(), vec![a.clone(), b.clone()]);
        // Removing returns the pid so the caller can kill it; a second remove is
        // a no-op returning None.
        assert_eq!(registry.remove(&a), Some(111));
        assert_eq!(registry.remove(&a), None);
        assert!(!registry.contains(&a));
        assert_eq!(registry.worktrees(), vec![b]);
    }

    #[test]
    fn terminal_registry_insert_replaces_a_previous_pid() {
        let mut registry = TerminalRegistry::new();
        let a = PathBuf::from("/a");
        registry.insert(a.clone(), 1);
        registry.insert(a.clone(), 2);
        assert_eq!(registry.pid(&a), Some(2));
    }

    #[test]
    fn attach_and_detach_return_actions() {
        let mut registry = SubscriberRegistry::new();
        let worktree = PathBuf::from("/repo/.usagi/sessions/work");
        assert_eq!(
            handle(
                ClientMessage::Attach {
                    worktree: worktree.clone()
                },
                1,
                &mut registry,
                &[]
            ),
            Action::Attach(worktree.clone())
        );
        assert_eq!(
            handle(
                ClientMessage::Detach {
                    worktree: worktree.clone()
                },
                1,
                &mut registry,
                &[]
            ),
            Action::Detach(worktree)
        );
    }

    #[test]
    fn keys_and_resize_return_terminal_io_actions() {
        let mut registry = SubscriberRegistry::new();
        let worktree = PathBuf::from("/repo/.usagi/sessions/work");
        assert_eq!(
            handle(
                ClientMessage::Keys {
                    worktree: worktree.clone(),
                    data: b"ls\n".to_vec(),
                },
                1,
                &mut registry,
                &[],
            ),
            Action::Keys(worktree.clone(), b"ls\n".to_vec())
        );
        assert_eq!(
            handle(
                ClientMessage::Resize {
                    worktree: worktree.clone(),
                    cols: 120,
                    rows: 40,
                },
                1,
                &mut registry,
                &[],
            ),
            Action::Resize(worktree, 120, 40)
        );
    }

    #[test]
    fn attach_table_tracks_multiple_clients_and_worktrees() {
        let mut table = AttachTable::new();
        let a = PathBuf::from("/a");
        let b = PathBuf::from("/b");
        table.attach(1, a.clone());
        table.attach(2, a.clone());
        table.attach(1, b.clone());
        assert!(table.is_attached(1, &a));
        assert_eq!(table.clients_for(&a), vec![1, 2]);
        assert_eq!(table.clients_for(&b), vec![1]);
        // No one attached to an unknown worktree.
        assert!(table.clients_for(&PathBuf::from("/none")).is_empty());
    }

    #[test]
    fn attach_table_detach_drops_worktree_when_last_client_leaves() {
        let mut table = AttachTable::new();
        let a = PathBuf::from("/a");
        table.attach(1, a.clone());
        table.attach(2, a.clone());
        table.detach(1, &a);
        assert!(!table.is_attached(1, &a));
        assert_eq!(table.clients_for(&a), vec![2]);
        table.detach(2, &a);
        assert!(table.clients_for(&a).is_empty());
        // Detaching from an unknown worktree is a no-op.
        table.detach(9, &PathBuf::from("/none"));
    }

    #[test]
    fn attach_table_remove_client_clears_it_everywhere() {
        let mut table = AttachTable::new();
        let a = PathBuf::from("/a");
        let b = PathBuf::from("/b");
        table.attach(1, a.clone());
        table.attach(2, a.clone());
        table.attach(1, b.clone());
        table.remove_client(1);
        assert_eq!(table.clients_for(&a), vec![2]);
        // `b` had only client 1, so it is gone entirely.
        assert!(table.clients_for(&b).is_empty());
    }
}

//! The daemon side of the IPC protocol: which connected clients want session
//! pushes, which are attached to which terminal, and how each incoming
//! [`ClientMessage`] is answered.
//!
//! This is the pure bookkeeping the socket server drives. The composition root
//! owns the sockets and the terminals; it hands each decoded message here with
//! the connection's id and the current snapshot, applies the returned reply, and
//! consults the registries when a snapshot or a terminal's output must be
//! pushed. Keeping the registries and the dispatch free of IO makes every branch
//! unit-testable without a socket.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::domain::daemon::SessionSnapshot;
use crate::domain::daemon_ipc::{ClientMessage, OutputBacklog, ServerMessage, TerminalId};

/// Identifies one connected client for the life of its connection. Assigned by
/// the socket server as connections are accepted.
pub type ClientId = u64;

/// What the socket server should do in response to a message, decided purely by
/// [`handle`]. Replies are sent as-is; the terminal actions carry real PTY IO the
/// composition root performs (spawning / killing / writing to the daemon-owned
/// process), which is why they are returned rather than executed here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Send this message back to the requesting client.
    Reply(ServerMessage),
    /// Spawn a new daemon-owned terminal with this launch configuration, then
    /// reply with its id.
    Spawn {
        worktree: PathBuf,
        command: Option<String>,
        env: BTreeMap<String, String>,
        cols: u16,
        rows: u16,
        scrollback: usize,
    },
    /// Kill this daemon-owned terminal, then reply.
    Kill(TerminalId),
    /// Attach the requesting client to this terminal's screen feed — after
    /// checking it runs in `worktree` — then send its current screen.
    Attach {
        terminal: TerminalId,
        worktree: PathBuf,
    },
    /// Detach the requesting client from this terminal's screen feed.
    Detach(TerminalId),
    /// Write these input bytes to this terminal.
    Keys(TerminalId, Vec<u8>),
    /// Resize this terminal to `cols`×`rows`.
    Resize(TerminalId, u16, u16),
    /// Nothing to send.
    Nothing,
}

/// One daemon-owned terminal as the registry tracks it: where it runs and the
/// pid of its shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalEntry {
    pub worktree: PathBuf,
    pub pid: u32,
}

/// The daemon-owned terminals, keyed by the [`TerminalId`] assigned at spawn.
/// Pure bookkeeping: the real PTY handles live in the composition root, which
/// mirrors its spawns and kills into this registry so the running set (and the
/// id→worktree resolution every terminal message needs) stays unit-testable.
/// Ids are never reused within a run, so a client's stale id can only miss, not
/// alias a different terminal.
#[derive(Debug)]
pub struct TerminalRegistry {
    next: TerminalId,
    terminals: HashMap<TerminalId, TerminalEntry>,
}

impl Default for TerminalRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            next: 1,
            terminals: HashMap::new(),
        }
    }

    /// Track a freshly spawned terminal, returning the id assigned to it.
    pub fn allocate(&mut self, worktree: PathBuf, pid: u32) -> TerminalId {
        let id = self.next;
        self.next += 1;
        self.terminals.insert(id, TerminalEntry { worktree, pid });
        id
    }

    /// Forget `terminal`, returning its entry if one was tracked.
    pub fn remove(&mut self, terminal: TerminalId) -> Option<TerminalEntry> {
        self.terminals.remove(&terminal)
    }

    /// The entry for `terminal`, if it is running.
    pub fn entry(&self, terminal: TerminalId) -> Option<&TerminalEntry> {
        self.terminals.get(&terminal)
    }

    /// Whether `terminal` is tracked and runs in `worktree` — the attach-time
    /// cross-check that keeps a stale persisted id from latching onto another
    /// worktree's terminal.
    pub fn belongs_to(&self, terminal: TerminalId, worktree: &Path) -> bool {
        self.terminals
            .get(&terminal)
            .is_some_and(|entry| entry.worktree == worktree)
    }

    /// The running terminal ids, sorted for a stable iteration order.
    pub fn ids(&self) -> Vec<TerminalId> {
        let mut ids: Vec<TerminalId> = self.terminals.keys().copied().collect();
        ids.sort_unstable();
        ids
    }
}

/// Which clients are attached to which terminal's screen feed, and how far into
/// that terminal's [`OutputBacklog`] each has been pushed. A client may be
/// attached to several terminals, and several clients may share one. Pure
/// bookkeeping the socket server consults when a terminal produces output.
#[derive(Debug, Default)]
pub struct AttachTable {
    by_terminal: HashMap<TerminalId, BTreeMap<ClientId, u64>>,
}

impl AttachTable {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach `client` to `terminal`'s screen feed with its cursor at the start;
    /// the caller advances it (see [`set_cursor`](Self::set_cursor)) once the
    /// initial snapshot is sent.
    pub fn attach(&mut self, client: ClientId, terminal: TerminalId) {
        self.by_terminal
            .entry(terminal)
            .or_default()
            .insert(client, 0);
    }

    /// Record that `client` has been pushed `terminal`'s output up to `cursor`.
    /// A no-op for a client that is not attached (it may have detached between
    /// the caller's snapshot and this update).
    pub fn set_cursor(&mut self, client: ClientId, terminal: TerminalId, cursor: u64) {
        if let Some(clients) = self.by_terminal.get_mut(&terminal) {
            if let Some(slot) = clients.get_mut(&client) {
                *slot = cursor;
            }
        }
    }

    /// Detach `client` from `terminal`, forgetting the terminal entirely once no
    /// client is attached to it.
    pub fn detach(&mut self, client: ClientId, terminal: TerminalId) {
        if let Some(clients) = self.by_terminal.get_mut(&terminal) {
            clients.remove(&client);
            if clients.is_empty() {
                self.by_terminal.remove(&terminal);
            }
        }
    }

    /// Remove `client` from every terminal — used when its connection drops.
    pub fn remove_client(&mut self, client: ClientId) {
        self.by_terminal.retain(|_, clients| {
            clients.remove(&client);
            !clients.is_empty()
        });
    }

    /// Forget `terminal` entirely (it exited or was killed), returning the
    /// clients that were attached so the caller can notify them.
    pub fn remove_terminal(&mut self, terminal: TerminalId) -> Vec<ClientId> {
        self.by_terminal
            .remove(&terminal)
            .map(|clients| clients.into_keys().collect())
            .unwrap_or_default()
    }

    /// The clients attached to `terminal` with their output cursors, in a
    /// stable (ascending id) order.
    pub fn clients_for(&self, terminal: TerminalId) -> Vec<(ClientId, u64)> {
        self.by_terminal
            .get(&terminal)
            .map(|clients| clients.iter().map(|(&id, &cursor)| (id, cursor)).collect())
            .unwrap_or_default()
    }

    /// Whether `client` is attached to `terminal`.
    pub fn is_attached(&self, client: ClientId, terminal: TerminalId) -> bool {
        self.by_terminal
            .get(&terminal)
            .is_some_and(|clients| clients.contains_key(&client))
    }
}

/// What one attached client should be sent to catch it up with a terminal's
/// output, decided by [`plan_screen_updates`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenUpdate {
    /// The raw output bytes the client is missing; its cursor advances to the
    /// backlog's end.
    Output(Vec<u8>),
    /// The client's cursor no longer addresses retained bytes — send a full
    /// screen snapshot and reset its cursor to the backlog's end.
    Snapshot,
}

/// Decide, for each attached client (with its current cursor), what to push so
/// it has seen `backlog`'s bytes up to the end. Fully caught-up clients get
/// nothing.
pub fn plan_screen_updates(
    backlog: &OutputBacklog,
    clients: &[(ClientId, u64)],
) -> Vec<(ClientId, ScreenUpdate)> {
    let end = backlog.end();
    clients
        .iter()
        .filter(|(_, cursor)| *cursor != end)
        .map(|&(client, cursor)| match backlog.since(cursor) {
            Some(data) => (client, ScreenUpdate::Output(data)),
            None => (client, ScreenUpdate::Snapshot),
        })
        .collect()
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
        ClientMessage::Spawn {
            worktree,
            command,
            env,
            cols,
            rows,
            scrollback,
        } => Action::Spawn {
            worktree,
            command,
            env,
            cols,
            rows,
            scrollback,
        },
        ClientMessage::Kill { terminal } => Action::Kill(terminal),
        ClientMessage::Attach { terminal, worktree } => Action::Attach { terminal, worktree },
        ClientMessage::Detach { terminal } => Action::Detach(terminal),
        ClientMessage::Keys { terminal, data } => Action::Keys(terminal, data),
        ClientMessage::Resize {
            terminal,
            cols,
            rows,
        } => Action::Resize(terminal, cols, rows),
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
    fn spawn_carries_the_full_launch_configuration() {
        let mut registry = SubscriberRegistry::new();
        let worktree = PathBuf::from("/repo/.usagi/sessions/work");
        let env: std::collections::BTreeMap<String, String> =
            [("TOKEN".to_string(), "secret".to_string())].into();
        assert_eq!(
            handle(
                ClientMessage::Spawn {
                    worktree: worktree.clone(),
                    command: Some("claude".to_string()),
                    env: env.clone(),
                    cols: 120,
                    rows: 40,
                    scrollback: 500,
                },
                1,
                &mut registry,
                &[]
            ),
            Action::Spawn {
                worktree,
                command: Some("claude".to_string()),
                env,
                cols: 120,
                rows: 40,
                scrollback: 500,
            }
        );
    }

    #[test]
    fn kill_returns_a_terminal_action() {
        let mut registry = SubscriberRegistry::new();
        assert_eq!(
            handle(ClientMessage::Kill { terminal: 3 }, 1, &mut registry, &[]),
            Action::Kill(3)
        );
    }

    #[test]
    fn terminal_registry_allocates_unique_ids_and_resolves_entries() {
        let mut registry = TerminalRegistry::new();
        let a = PathBuf::from("/a");
        let b = PathBuf::from("/b");
        let id_a = registry.allocate(a.clone(), 111);
        let id_b = registry.allocate(b.clone(), 222);
        assert_ne!(id_a, id_b);
        assert_eq!(
            registry.entry(id_a),
            Some(&TerminalEntry {
                worktree: a.clone(),
                pid: 111
            })
        );
        assert_eq!(registry.ids(), vec![id_a, id_b]);
        // Removing returns the entry so the caller can kill it; a second remove
        // is a no-op returning None.
        assert_eq!(
            registry.remove(id_a),
            Some(TerminalEntry {
                worktree: a,
                pid: 111
            })
        );
        assert_eq!(registry.remove(id_a), None);
        assert_eq!(registry.entry(id_a), None);
        assert_eq!(registry.ids(), vec![id_b]);
    }

    #[test]
    fn terminal_registry_default_is_empty_and_allocates_from_one() {
        let mut registry = TerminalRegistry::default();
        assert!(registry.ids().is_empty());
        assert_eq!(registry.allocate(PathBuf::from("/a"), 1), 1);
    }

    #[test]
    fn terminal_registry_never_reuses_an_id_after_removal() {
        let mut registry = TerminalRegistry::new();
        let first = registry.allocate(PathBuf::from("/a"), 1);
        registry.remove(first);
        let second = registry.allocate(PathBuf::from("/a"), 2);
        assert_ne!(first, second);
    }

    #[test]
    fn terminal_registry_checks_worktree_ownership() {
        let mut registry = TerminalRegistry::new();
        let id = registry.allocate(PathBuf::from("/a"), 1);
        assert!(registry.belongs_to(id, Path::new("/a")));
        assert!(!registry.belongs_to(id, Path::new("/b")));
        assert!(!registry.belongs_to(99, Path::new("/a")));
    }

    #[test]
    fn attach_and_detach_return_actions() {
        let mut registry = SubscriberRegistry::new();
        let worktree = PathBuf::from("/repo/.usagi/sessions/work");
        assert_eq!(
            handle(
                ClientMessage::Attach {
                    terminal: 5,
                    worktree: worktree.clone()
                },
                1,
                &mut registry,
                &[]
            ),
            Action::Attach {
                terminal: 5,
                worktree
            }
        );
        assert_eq!(
            handle(ClientMessage::Detach { terminal: 5 }, 1, &mut registry, &[]),
            Action::Detach(5)
        );
    }

    #[test]
    fn keys_and_resize_return_terminal_io_actions() {
        let mut registry = SubscriberRegistry::new();
        assert_eq!(
            handle(
                ClientMessage::Keys {
                    terminal: 4,
                    data: b"ls\n".to_vec(),
                },
                1,
                &mut registry,
                &[],
            ),
            Action::Keys(4, b"ls\n".to_vec())
        );
        assert_eq!(
            handle(
                ClientMessage::Resize {
                    terminal: 4,
                    cols: 120,
                    rows: 40,
                },
                1,
                &mut registry,
                &[],
            ),
            Action::Resize(4, 120, 40)
        );
    }

    #[test]
    fn attach_table_tracks_multiple_clients_and_terminals() {
        let mut table = AttachTable::new();
        table.attach(1, 10);
        table.attach(2, 10);
        table.attach(1, 20);
        assert!(table.is_attached(1, 10));
        assert_eq!(table.clients_for(10), vec![(1, 0), (2, 0)]);
        assert_eq!(table.clients_for(20), vec![(1, 0)]);
        // No one attached to an unknown terminal.
        assert!(table.clients_for(99).is_empty());
    }

    #[test]
    fn attach_table_tracks_per_client_cursors() {
        let mut table = AttachTable::new();
        table.attach(1, 10);
        table.attach(2, 10);
        table.set_cursor(1, 10, 42);
        assert_eq!(table.clients_for(10), vec![(1, 42), (2, 0)]);
        // Advancing a cursor for a client that is not attached is a no-op.
        table.set_cursor(3, 10, 7);
        table.set_cursor(1, 99, 7);
        assert_eq!(table.clients_for(10), vec![(1, 42), (2, 0)]);
        assert!(table.clients_for(99).is_empty());
    }

    #[test]
    fn attach_table_detach_drops_terminal_when_last_client_leaves() {
        let mut table = AttachTable::new();
        table.attach(1, 10);
        table.attach(2, 10);
        table.detach(1, 10);
        assert!(!table.is_attached(1, 10));
        assert_eq!(table.clients_for(10), vec![(2, 0)]);
        table.detach(2, 10);
        assert!(table.clients_for(10).is_empty());
        // Detaching from an unknown terminal is a no-op.
        table.detach(9, 99);
    }

    #[test]
    fn attach_table_remove_client_clears_it_everywhere() {
        let mut table = AttachTable::new();
        table.attach(1, 10);
        table.attach(2, 10);
        table.attach(1, 20);
        table.remove_client(1);
        assert_eq!(table.clients_for(10), vec![(2, 0)]);
        // Terminal 20 had only client 1, so it is gone entirely.
        assert!(table.clients_for(20).is_empty());
    }

    #[test]
    fn attach_table_remove_terminal_reports_its_attachers() {
        let mut table = AttachTable::new();
        table.attach(1, 10);
        table.attach(2, 10);
        let mut notified = table.remove_terminal(10);
        notified.sort_unstable();
        assert_eq!(notified, vec![1, 2]);
        assert!(table.clients_for(10).is_empty());
        // Removing an unknown terminal notifies no one.
        assert!(table.remove_terminal(99).is_empty());
    }

    #[test]
    fn plan_sends_missing_bytes_and_skips_caught_up_clients() {
        let mut backlog = OutputBacklog::new(16);
        backlog.append(b"hello");
        // Client 1 saw nothing yet, client 2 saw "hel", client 3 is caught up.
        let plan = plan_screen_updates(&backlog, &[(1, 0), (2, 3), (3, 5)]);
        assert_eq!(
            plan,
            vec![
                (1, ScreenUpdate::Output(b"hello".to_vec())),
                (2, ScreenUpdate::Output(b"lo".to_vec())),
            ]
        );
    }

    #[test]
    fn plan_resyncs_a_client_that_fell_past_the_backlog() {
        let mut backlog = OutputBacklog::new(4);
        backlog.append(b"abcdef");
        // Start is now 2: a cursor at 0 addresses evicted bytes.
        let plan = plan_screen_updates(&backlog, &[(1, 0)]);
        assert_eq!(plan, vec![(1, ScreenUpdate::Snapshot)]);
    }
}

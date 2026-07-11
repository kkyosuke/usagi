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
    /// Compare this client build with the daemon build, then reply with the
    /// daemon identity supplied by the composition root.
    Hello { build: String },
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
    /// Send the requesting client a scrollback viewport snapshot.
    Scrollback(TerminalId, usize),
    /// Nothing to send.
    Nothing,
}

impl Action {
    /// Whether this action may touch a daemon-owned terminal and therefore
    /// requires a successful executable-generation handshake on its connection.
    pub fn requires_build_handshake(&self) -> bool {
        matches!(
            self,
            Self::Spawn { .. }
                | Self::Kill(_)
                | Self::Attach { .. }
                | Self::Detach(_)
                | Self::Keys(_, _)
                | Self::Resize(_, _, _)
                | Self::Scrollback(_, _)
        )
    }
}

/// Whether a client's executable generation is safe to combine with the daemon.
pub fn builds_match(client: &str, daemon: &str) -> bool {
    client == daemon
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
        self.insert_known(id, worktree, pid);
        id
    }

    /// Track a terminal whose id is already known, advancing the allocator past
    /// it so future spawns cannot reuse an adopted id.
    pub fn insert_known(&mut self, terminal: TerminalId, worktree: PathBuf, pid: u32) {
        self.next = self.next.max(terminal.saturating_add(1));
        self.terminals
            .insert(terminal, TerminalEntry { worktree, pid });
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

/// One client's current view of an attached terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientViewport {
    pub cursor: u64,
    pub scrollback: usize,
    pub primary_high_water: u64,
}

/// Which clients are attached to which terminal's screen feed, how far into
/// that terminal's [`OutputBacklog`] each has been pushed, and what scrollback
/// viewport each currently displays. A client may be attached to several
/// terminals, and several clients may share one. Pure bookkeeping the socket
/// server consults when a terminal produces output.
#[derive(Debug, Default)]
pub struct AttachTable {
    by_terminal: HashMap<TerminalId, BTreeMap<ClientId, ClientViewport>>,
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
        self.by_terminal.entry(terminal).or_default().insert(
            client,
            ClientViewport {
                cursor: 0,
                scrollback: 0,
                primary_high_water: 0,
            },
        );
    }

    /// Record that `client` has been pushed `terminal`'s output up to `cursor`.
    /// A no-op for a client that is not attached (it may have detached between
    /// the caller's snapshot and this update).
    pub fn set_cursor(&mut self, client: ClientId, terminal: TerminalId, cursor: u64) {
        if let Some(clients) = self.by_terminal.get_mut(&terminal) {
            if let Some(viewport) = clients.get_mut(&client) {
                viewport.cursor = cursor;
            }
        }
    }

    /// Record the viewport offset and history watermark represented by the
    /// latest snapshot sent to `client`.
    pub fn set_viewport(
        &mut self,
        client: ClientId,
        terminal: TerminalId,
        scrollback: usize,
        primary_high_water: u64,
    ) {
        if let Some(clients) = self.by_terminal.get_mut(&terminal) {
            if let Some(viewport) = clients.get_mut(&client) {
                viewport.scrollback = scrollback;
                viewport.primary_high_water = primary_high_water;
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

    /// The clients attached to `terminal` with their viewports, in a stable
    /// (ascending id) order.
    pub fn clients_for(&self, terminal: TerminalId) -> Vec<(ClientId, ClientViewport)> {
        self.by_terminal
            .get(&terminal)
            .map(|clients| {
                clients
                    .iter()
                    .map(|(&id, &viewport)| (id, viewport))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether `client` is attached to `terminal`.
    pub fn is_attached(&self, client: ClientId, terminal: TerminalId) -> bool {
        self.by_terminal
            .get(&terminal)
            .is_some_and(|clients| clients.contains_key(&client))
    }

    /// Whether any client is attached to `terminal`.
    pub fn has_terminal(&self, terminal: TerminalId) -> bool {
        self.by_terminal
            .get(&terminal)
            .is_some_and(|clients| !clients.is_empty())
    }

    /// Terminal ids that currently have at least one attached client.
    pub fn terminals(&self) -> Vec<TerminalId> {
        self.by_terminal.keys().copied().collect()
    }
}

/// A terminal record persisted so a restarted daemon can decide which child
/// processes survived an abnormal daemon exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedTerminal {
    pub terminal: TerminalId,
    pub worktree: PathBuf,
    pub pid: u32,
}

/// Split persisted terminal records into live processes to adopt and dead/stale
/// records to discard. The caller supplies the process-table check so this stays
/// pure and unit-testable.
pub fn plan_adopt_terminals(
    persisted: &[PersistedTerminal],
    alive: &dyn Fn(u32) -> bool,
) -> (Vec<PersistedTerminal>, Vec<PersistedTerminal>) {
    persisted
        .iter()
        .cloned()
        .partition(|terminal| alive(terminal.pid))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityNoticeKind {
    Waiting,
    Done,
}

/// Whether a daemon-side notification should fire for a session activity
/// transition, given whether the session currently has an attached TUI.
pub fn should_notify_activity(
    previous: Option<crate::domain::daemon::SessionActivity>,
    current: Option<crate::domain::daemon::SessionActivity>,
    attached: bool,
) -> Option<ActivityNoticeKind> {
    use crate::domain::daemon::SessionActivity;

    match (previous, current) {
        (prev, Some(SessionActivity::Waiting)) if prev != Some(SessionActivity::Waiting) => {
            Some(ActivityNoticeKind::Waiting)
        }
        (prev, Some(SessionActivity::Done)) if prev != Some(SessionActivity::Done) && !attached => {
            Some(ActivityNoticeKind::Done)
        }
        _ => None,
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
    /// screen snapshot and reset its cursor to the backlog's end. `offset` is
    /// the scrollback viewport the snapshot should represent.
    Snapshot { offset: usize },
}

/// Decide, for each attached client (with its current cursor), what to push so
/// it has seen `backlog`'s bytes up to the end. Fully caught-up clients get
/// nothing.
pub fn plan_screen_updates(
    backlog: &OutputBacklog,
    clients: &[(ClientId, ClientViewport)],
    primary_high_water: u64,
) -> Vec<(ClientId, ScreenUpdate)> {
    let end = backlog.end();
    clients
        .iter()
        .filter(|(_, viewport)| viewport.cursor != end)
        .map(|&(client, viewport)| {
            if viewport.scrollback > 0 {
                let added = primary_high_water.saturating_sub(viewport.primary_high_water);
                let offset = viewport.scrollback.saturating_add(added as usize);
                return (client, ScreenUpdate::Snapshot { offset });
            }
            match backlog.since(viewport.cursor) {
                Some(data) => (client, ScreenUpdate::Output(data)),
                None => (client, ScreenUpdate::Snapshot { offset: 0 }),
            }
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
        ClientMessage::Hello { build } => Action::Hello { build },
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
        ClientMessage::Scrollback { terminal, offset } => Action::Scrollback(terminal, offset),
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
            worktree: None,
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
    fn hello_requests_the_composition_roots_build_identity() {
        let mut registry = SubscriberRegistry::new();
        assert_eq!(
            handle(
                ClientMessage::Hello {
                    build: "client-build".to_string(),
                },
                7,
                &mut registry,
                &sample_sessions(),
            ),
            Action::Hello {
                build: "client-build".to_string()
            }
        );
        assert!(!registry.is_subscribed(7));
    }

    #[test]
    fn build_policy_guards_every_terminal_action() {
        let env = BTreeMap::new();
        let terminal_actions = [
            Action::Spawn {
                worktree: PathBuf::from("/repo"),
                command: None,
                env,
                cols: 80,
                rows: 24,
                scrollback: 100,
            },
            Action::Kill(1),
            Action::Attach {
                terminal: 1,
                worktree: PathBuf::from("/repo"),
            },
            Action::Detach(1),
            Action::Keys(1, Vec::new()),
            Action::Resize(1, 80, 24),
            Action::Scrollback(1, 0),
        ];
        assert!(terminal_actions
            .iter()
            .all(Action::requires_build_handshake));
        assert!(!Action::Hello {
            build: "build".to_string()
        }
        .requires_build_handshake());
        assert!(!Action::Reply(ServerMessage::Sessions {
            sessions: Vec::new()
        })
        .requires_build_handshake());
        assert!(!Action::Nothing.requires_build_handshake());

        assert!(builds_match("same", "same"));
        assert!(!builds_match("old", "new"));
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
    fn terminal_registry_advances_past_adopted_ids() {
        let mut registry = TerminalRegistry::new();
        registry.insert_known(9, PathBuf::from("/old"), 99);
        assert_eq!(registry.allocate(PathBuf::from("/new"), 100), 10);
        assert_eq!(
            registry.entry(9),
            Some(&TerminalEntry {
                worktree: PathBuf::from("/old"),
                pid: 99
            })
        );
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
    fn scrollback_returns_a_viewport_action() {
        let mut registry = SubscriberRegistry::new();
        assert_eq!(
            handle(
                ClientMessage::Scrollback {
                    terminal: 4,
                    offset: 12,
                },
                1,
                &mut registry,
                &[],
            ),
            Action::Scrollback(4, 12)
        );
    }

    #[test]
    fn attach_table_tracks_multiple_clients_and_terminals() {
        let mut table = AttachTable::new();
        table.attach(1, 10);
        table.attach(2, 10);
        table.attach(1, 20);
        assert!(table.is_attached(1, 10));
        assert_eq!(
            table.clients_for(10),
            vec![
                (
                    1,
                    ClientViewport {
                        cursor: 0,
                        scrollback: 0,
                        primary_high_water: 0,
                    }
                ),
                (
                    2,
                    ClientViewport {
                        cursor: 0,
                        scrollback: 0,
                        primary_high_water: 0,
                    }
                )
            ]
        );
        assert_eq!(
            table.clients_for(20),
            vec![(
                1,
                ClientViewport {
                    cursor: 0,
                    scrollback: 0,
                    primary_high_water: 0,
                }
            )]
        );
        // No one attached to an unknown terminal.
        assert!(table.clients_for(99).is_empty());
    }

    #[test]
    fn attach_table_tracks_per_client_cursors() {
        let mut table = AttachTable::new();
        table.attach(1, 10);
        table.attach(2, 10);
        table.set_cursor(1, 10, 42);
        assert_eq!(table.clients_for(10)[0].1.cursor, 42);
        assert_eq!(table.clients_for(10)[1].1.cursor, 0);
        // Advancing a cursor for a client that is not attached is a no-op.
        table.set_cursor(3, 10, 7);
        table.set_cursor(1, 99, 7);
        assert_eq!(table.clients_for(10)[0].1.cursor, 42);
        assert!(table.clients_for(99).is_empty());
    }

    #[test]
    fn attach_table_tracks_viewport_metadata() {
        let mut table = AttachTable::new();
        table.attach(1, 10);
        table.set_viewport(1, 10, 5, 90);
        assert_eq!(
            table.clients_for(10),
            vec![(
                1,
                ClientViewport {
                    cursor: 0,
                    scrollback: 5,
                    primary_high_water: 90,
                }
            )]
        );
        table.set_viewport(2, 10, 9, 99);
        table.set_viewport(1, 20, 9, 99);
        assert_eq!(table.clients_for(10)[0].1.scrollback, 5);
    }

    #[test]
    fn attach_table_detach_drops_terminal_when_last_client_leaves() {
        let mut table = AttachTable::new();
        table.attach(1, 10);
        table.attach(2, 10);
        table.detach(1, 10);
        assert!(!table.is_attached(1, 10));
        assert_eq!(table.clients_for(10)[0].0, 2);
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
        assert_eq!(table.clients_for(10)[0].0, 2);
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
    fn attach_table_reports_whether_a_terminal_has_viewers() {
        let mut table = AttachTable::new();
        assert!(!table.has_terminal(10));
        table.attach(1, 10);
        assert!(table.has_terminal(10));
        table.detach(1, 10);
        assert!(!table.has_terminal(10));
    }

    #[test]
    fn attach_table_lists_only_viewed_terminals() {
        let mut table = AttachTable::new();
        table.attach(1, 10);
        table.attach(2, 20);
        table.attach(3, 10);
        let mut terminals = table.terminals();
        terminals.sort_unstable();
        assert_eq!(terminals, vec![10, 20]);
        table.detach(1, 10);
        table.detach(3, 10);
        assert_eq!(table.terminals(), vec![20]);
    }

    #[test]
    fn adopt_plan_keeps_live_processes_and_discards_dead_records() {
        let records = vec![
            PersistedTerminal {
                terminal: 1,
                worktree: PathBuf::from("/live"),
                pid: 10,
            },
            PersistedTerminal {
                terminal: 2,
                worktree: PathBuf::from("/dead"),
                pid: 20,
            },
        ];
        let (adopt, discard) = plan_adopt_terminals(&records, &|pid| pid == 10);
        assert_eq!(adopt, vec![records[0].clone()]);
        assert_eq!(discard, vec![records[1].clone()]);
    }

    #[test]
    fn notification_policy_waiting_always_fires_but_done_is_background_only() {
        use crate::domain::daemon::SessionActivity;

        assert_eq!(
            should_notify_activity(None, Some(SessionActivity::Waiting), true),
            Some(ActivityNoticeKind::Waiting)
        );
        assert_eq!(
            should_notify_activity(
                Some(SessionActivity::Running),
                Some(SessionActivity::Done),
                false
            ),
            Some(ActivityNoticeKind::Done)
        );
        assert_eq!(
            should_notify_activity(
                Some(SessionActivity::Running),
                Some(SessionActivity::Done),
                true
            ),
            None
        );
        assert_eq!(
            should_notify_activity(
                Some(SessionActivity::Waiting),
                Some(SessionActivity::Waiting),
                false
            ),
            None
        );
    }

    #[test]
    fn plan_sends_missing_bytes_and_skips_caught_up_clients() {
        let mut backlog = OutputBacklog::new(16);
        backlog.append(b"hello");
        // Client 1 saw nothing yet, client 2 saw "hel", client 3 is caught up.
        let plan = plan_screen_updates(
            &backlog,
            &[
                (
                    1,
                    ClientViewport {
                        cursor: 0,
                        scrollback: 0,
                        primary_high_water: 0,
                    },
                ),
                (
                    2,
                    ClientViewport {
                        cursor: 3,
                        scrollback: 0,
                        primary_high_water: 0,
                    },
                ),
                (
                    3,
                    ClientViewport {
                        cursor: 5,
                        scrollback: 0,
                        primary_high_water: 0,
                    },
                ),
            ],
            0,
        );
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
        let plan = plan_screen_updates(
            &backlog,
            &[(
                1,
                ClientViewport {
                    cursor: 0,
                    scrollback: 0,
                    primary_high_water: 0,
                },
            )],
            0,
        );
        assert_eq!(plan, vec![(1, ScreenUpdate::Snapshot { offset: 0 })]);
    }

    #[test]
    fn plan_snapshots_scrolled_clients_and_advances_their_offset() {
        let mut backlog = OutputBacklog::new(16);
        backlog.append(b"old");
        let cursor = backlog.end();
        backlog.append(b"new");
        let plan = plan_screen_updates(
            &backlog,
            &[(
                1,
                ClientViewport {
                    cursor,
                    scrollback: 3,
                    primary_high_water: 10,
                },
            )],
            14,
        );
        assert_eq!(plan, vec![(1, ScreenUpdate::Snapshot { offset: 7 })]);
    }
}

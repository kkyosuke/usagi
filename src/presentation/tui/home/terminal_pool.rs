//! A pool of live embedded terminals, one per worktree directory.
//!
//! The workspace screen embeds at most one shell per worktree in its right
//! pane. To let the user switch sessions while a `terminal` or `agent` keeps
//! running, the [`PtySession`]s cannot live on the stack of the terminal loop
//! (where leaving would drop — and so kill — them). Instead they are owned here,
//! keyed by worktree directory, for the lifetime of the screen: detaching
//! (`Ctrl-O`) returns to the sidebar but leaves the shell — and any agent CLI
//! running inside it — alive in the pool, so re-attaching later finds it exactly
//! where it was left.
//!
//! Because those shells keep running in the background, the pool also watches
//! them: a background thread polls every session through a [`SessionMonitor`].
//! For each it reads two signals — the phase the agent's lifecycle hooks
//! recorded (via [`agent_state_store`]) and the shell's bell count — and lets
//! the monitor decide (the phase wins; the bell is the fallback for agents
//! without hooks). When a **background** session starts waiting (`◆`) or its
//! agent finishes (`✓`), the watcher fires a one-shot desktop notification. The
//! per-session state is exposed through a [`MonitorHandle`] the render loops read
//! to mark the session in the sidebar. The attached session is shown with the
//! same state as anywhere else (it is seen live) — being attached only suppresses
//! its notification, not its badge.
//!
//! This is pure I/O and process ownership (it spawns shells, holds their
//! handles, runs a watcher thread, and shows desktop notifications), so — like
//! [`PtySession`] itself — it is excluded from coverage. Its pure core, the
//! waiting-state bookkeeping, lives in [`SessionMonitor`] and is tested there.
//! The geometry it spawns at ([`super::ui::terminal_geometry`]) is tested on its
//! own.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Result;
use console::Term;

use crate::infrastructure::pty::PtySession;
use crate::infrastructure::session_monitor::SessionMonitor;
use crate::infrastructure::{agent_state_store, session_monitor};

use super::terminal_tabs::{self, PaneKind, TabNav};
use super::terminal_view::TerminalView;
use super::ui;

/// How often the watcher thread samples every session's bell count.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// The handles a background session is watched through, kept separate from the
/// owned [`PtySession`]s so the watcher thread can poll without holding them.
///
/// A session can hold several panes at once (an agent alongside one or more
/// terminals), so liveness is the union of every pane's flag, while the bell /
/// phase heuristic follows the pane that matters — the agent pane, whose
/// lifecycle drives the sidebar badge, or the representative pane when there is
/// no agent.
struct Watched {
    /// Every pane's running bell count is shared with its [`PtySession`]; this is
    /// the one the monitor heuristic reads — the agent pane's when present.
    bell: Arc<AtomicU64>,
    /// Every pane's liveness flag; the session is live while any is `true`.
    alive: Vec<Arc<AtomicBool>>,
    /// A human label (the worktree branch) shown in the notification.
    label: String,
}

impl Watched {
    /// Whether the session still has at least one live pane.
    fn any_alive(&self) -> bool {
        self.alive.iter().any(|a| a.load(Ordering::SeqCst))
    }
}

/// State shared between the pool, the watcher thread, and the render loops.
#[derive(Default)]
struct Shared {
    monitor: SessionMonitor,
    sessions: HashMap<PathBuf, Watched>,
}

/// A cloneable read/notify handle onto the shared waiting state, given to the
/// render loops so they can mark waiting sessions and declare which session is
/// in the foreground.
#[derive(Clone)]
pub struct MonitorHandle {
    shared: Arc<Mutex<Shared>>,
}

/// Every session-badge set the sidebar draws, read together under one lock by
/// [`MonitorHandle::snapshot`]. Comparing two snapshots tells a render loop
/// whether the badges changed since its last paint, so an idle pane can skip the
/// repaint entirely.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MonitorSnapshot {
    /// Worktree paths whose agent is actively working a turn.
    pub running: HashSet<PathBuf>,
    /// Worktree paths currently waiting for the user.
    pub waiting: HashSet<PathBuf>,
    /// Worktree paths whose agent has finished (a turn completed or it exited).
    pub done: HashSet<PathBuf>,
    /// Worktree paths with a live embedded session — a shell, and any agent CLI
    /// inside it, still alive whether attached or left running in the background.
    pub live: HashSet<PathBuf>,
}

impl MonitorHandle {
    /// A handle backed by empty state and no watcher — for screens (and tests)
    /// that render without any live sessions.
    pub fn detached() -> Self {
        Self {
            shared: Arc::new(Mutex::new(Shared::default())),
        }
    }

    /// A handle reporting the given paths as live (running) sessions, for tests
    /// that exercise the quit-confirmation flow without spawning a real shell.
    #[cfg(test)]
    pub fn with_live(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        let mut shared = Shared::default();
        for path in paths {
            shared.sessions.insert(
                path,
                Watched {
                    bell: Arc::new(AtomicU64::new(0)),
                    alive: vec![Arc::new(AtomicBool::new(true))],
                    label: String::new(),
                },
            );
        }
        Self {
            shared: Arc::new(Mutex::new(shared)),
        }
    }

    /// Read every session-badge set the sidebar needs for one repaint under a
    /// single lock, instead of locking once per set. The render loops took four
    /// separate locks (`running`/`waiting`/`done`/`live`) each frame, contending
    /// with the watcher thread that holds the same mutex; one lock per repaint
    /// removes that. The returned [`MonitorSnapshot`] is comparable, so a caller
    /// can also skip repainting when the badges have not changed.
    pub fn snapshot(&self) -> MonitorSnapshot {
        let shared = self.lock();
        let live = shared
            .sessions
            .iter()
            .filter(|(_, w)| w.any_alive())
            .map(|(path, _)| path.clone())
            .collect();
        MonitorSnapshot {
            running: shared.monitor.running().clone(),
            waiting: shared.monitor.waiting().clone(),
            done: shared.monitor.done().clone(),
            live,
        }
    }

    /// Declare the foreground (attached) session, or clear it with `None`. The
    /// attached session is shown with its true state like any other; being
    /// attached only suppresses its desktop notification and the bell heuristic.
    pub fn set_attached(&self, path: Option<PathBuf>) {
        self.lock().monitor.set_attached(path);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().expect("terminal monitor mutex poisoned")
    }
}

/// One embedded pane: a live [`PtySession`] and what it runs (so the tab strip
/// can label it and the agent pane can be told apart for the badge heuristic).
struct Pane {
    pty: PtySession,
    kind: PaneKind,
}

/// The panes of one session (worktree), in tab order, and which one is active
/// (visible / driven). A session keeps every pane alive in the background; only
/// the active one is attached at a time.
struct SessionPanes {
    panes: Vec<Pane>,
    active: usize,
}

/// The live shells embedded in the workspace screen, keyed by worktree path —
/// each path holding one or more panes (an agent alongside any terminals).
///
/// Owned by the screen ([`super::run`]); dropped when the user leaves it, which
/// kills every shell it still holds (via [`PtySession`]'s `Drop`) and stops the
/// watcher thread.
pub struct TerminalPool {
    sessions: HashMap<PathBuf, SessionPanes>,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    watcher: Option<JoinHandle<()>>,
}

impl TerminalPool {
    /// An empty pool with its watcher thread running. `notifications_enabled`
    /// gates the desktop notification fired when a detached session starts
    /// waiting for input.
    pub fn new(notifications_enabled: bool) -> Self {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let watcher = spawn_watcher(
            Arc::clone(&shared),
            Arc::clone(&stop),
            notifications_enabled,
        );
        Self {
            sessions: HashMap::new(),
            shared,
            stop,
            watcher: Some(watcher),
        }
    }

    /// A handle the render loops read to mark waiting sessions and to declare
    /// the foreground session.
    pub fn monitor(&self) -> MonitorHandle {
        MonitorHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Make `dir`'s active pane ready to attach. With no live pane yet, spawns the
    /// first one — an agent pane (when `agent`, sending `agent_command` once on
    /// spawn) or a plain terminal — and makes it active. With panes already alive,
    /// this re-attaches to the one the user left active, ignoring `agent`: a
    /// second kind is added explicitly from inside the pane ([`add_pane`], `Ctrl-O
    /// t` / `Ctrl-O a`), not by re-entering the session. `label` (the worktree
    /// branch) is shown in the waiting notification.
    ///
    /// [`add_pane`]: Self::add_pane
    pub fn enter(
        &mut self,
        term: &Term,
        dir: &Path,
        agent: bool,
        agent_command: Option<&str>,
        label: &str,
    ) -> Result<()> {
        let key = dir.to_path_buf();
        let alive = self
            .sessions
            .get(&key)
            .is_some_and(|sp| sp.panes.iter().any(|p| p.pty.is_alive()));
        if alive {
            // Re-attach: clamp the active index defensively in case panes changed.
            if let Some(sp) = self.sessions.get_mut(&key) {
                sp.active = sp.active.min(sp.panes.len().saturating_sub(1));
            }
        } else {
            // No live pane (fresh session, or every pane exited): drop any stale
            // entry and spawn the first pane of the requested kind.
            self.sessions.remove(&key);
            let kind = pane_kind(agent);
            let pane = self.spawn_pane(term, dir, kind, agent_command)?;
            self.sessions.insert(
                key,
                SessionPanes {
                    panes: vec![pane],
                    active: 0,
                },
            );
        }
        self.refresh_watched(dir, label);
        Ok(())
    }

    /// Spawn a new pane of `kind` for `dir` and make it the active tab — the
    /// `Ctrl-O t` / `Ctrl-O a` path, which always adds a pane (so a session can
    /// hold an agent alongside one or more terminals). An agent pane sends
    /// `agent_command` once on spawn; a terminal pane opens a plain shell.
    pub fn add_pane(
        &mut self,
        term: &Term,
        dir: &Path,
        kind: PaneKind,
        agent_command: Option<&str>,
        label: &str,
    ) -> Result<()> {
        let pane = self.spawn_pane(term, dir, kind, agent_command)?;
        let sp = self
            .sessions
            .entry(dir.to_path_buf())
            .or_insert_with(|| SessionPanes {
                panes: Vec::new(),
                active: 0,
            });
        sp.panes.push(pane);
        sp.active = sp.panes.len() - 1;
        self.refresh_watched(dir, label);
        Ok(())
    }

    /// Move the active tab within `dir` (next / previous / a numbered jump),
    /// leaving every pane alive. A no-op for a session with no panes.
    pub fn nav(&mut self, dir: &Path, nav: TabNav) {
        if let Some(sp) = self.sessions.get_mut(dir) {
            sp.active = terminal_tabs::resolve_nav(sp.active, sp.panes.len(), nav);
        }
    }

    /// Close `dir`'s active pane, killing its shell (its [`PtySession`] drops).
    /// Returns whether any pane remains: `true` leaves the next tab active so the
    /// caller keeps driving, `false` means the session is empty and the caller
    /// drops back to 在席. The whole session entry is removed when it empties.
    pub fn close_active(&mut self, dir: &Path, label: &str) -> bool {
        let key = dir.to_path_buf();
        let remains = match self.sessions.get_mut(&key) {
            Some(sp) if !sp.panes.is_empty() => {
                let active = sp.active.min(sp.panes.len() - 1);
                let len_before = sp.panes.len();
                // Dropping the removed Pane kills the shell it owns.
                sp.panes.remove(active);
                match terminal_tabs::active_after_close(active, len_before) {
                    Some(next) => {
                        sp.active = next;
                        true
                    }
                    None => false,
                }
            }
            _ => false,
        };
        if !remains {
            self.sessions.remove(&key);
        }
        self.refresh_watched(dir, label);
        remains
    }

    /// Whether `dir` already has a live pane — so re-entering the session would
    /// re-attach an existing pane rather than freshly spawn one. The home screen
    /// reads this to decide when a fresh agent spawn will happen, and so when a
    /// prompt queued for the session should be consumed (mirrors the `alive`
    /// check in [`enter`](Self::enter)).
    pub fn has_live_pane(&self, dir: &Path) -> bool {
        self.sessions
            .get(dir)
            .is_some_and(|sp| sp.panes.iter().any(|p| p.pty.is_alive()))
    }

    /// Borrow `dir`'s active pane's shell, or `None` when the session has no
    /// panes — the pane the terminal loop drives.
    pub fn active_pty(&mut self, dir: &Path) -> Option<&mut PtySession> {
        let sp = self.sessions.get_mut(dir)?;
        if sp.panes.is_empty() {
            return None;
        }
        let active = sp.active.min(sp.panes.len() - 1);
        Some(&mut sp.panes[active].pty)
    }

    /// The tab strip for `dir`: a label per pane (in tab order) and the active
    /// index, for the renderer to draw above the embedded terminal. Empty when no
    /// session is rooted there.
    pub fn tabs(&self, dir: &Path) -> (Vec<String>, usize) {
        match self.sessions.get(dir) {
            Some(sp) => {
                let kinds: Vec<PaneKind> = sp.panes.iter().map(|p| p.kind).collect();
                let active = sp.active.min(sp.panes.len().saturating_sub(1));
                (terminal_tabs::tab_labels(&kinds), active)
            }
            None => (Vec::new(), 0),
        }
    }

    /// Spawn one pane: size it to the attached (tab-strip-reserved) geometry, send
    /// the agent CLI on an agent pane's first spawn, and — for an agent pane —
    /// clear any phase a previous agent at this worktree recorded so the fresh
    /// pane does not inherit a stale running / waiting state before its own hooks
    /// fire. The launch command is handed to the shell as an argument (not typed
    /// into its stdin) so it is never echoed before the agent draws (see
    /// [`PtySession::spawn`]).
    fn spawn_pane(
        &self,
        term: &Term,
        dir: &Path,
        kind: PaneKind,
        agent_command: Option<&str>,
    ) -> Result<Pane> {
        let (height, width) = term.size();
        let geo = ui::attached_geometry(height as usize, width as usize);
        let initial = match kind {
            PaneKind::Agent => agent_command,
            PaneKind::Terminal => None,
        };
        if matches!(kind, PaneKind::Agent) {
            agent_state_store::clear(dir);
            self.lock().monitor.forget(dir);
        }
        let pty = PtySession::spawn(dir, geo.rows, geo.cols, initial)?;
        Ok(Pane { pty, kind })
    }

    /// Re-register `dir`'s watched handles from its current panes: liveness is the
    /// union of every pane's flag, and the bell the monitor heuristic reads is the
    /// agent pane's (or the first pane's when there is none). When the session has
    /// no panes left it is forgotten — its watched / monitor / phase state cleared.
    fn refresh_watched(&self, dir: &Path, label: &str) {
        let key = dir.to_path_buf();
        let watched = self.sessions.get(&key).and_then(|sp| {
            let bell = sp
                .panes
                .iter()
                .find(|p| matches!(p.kind, PaneKind::Agent))
                .or_else(|| sp.panes.first())
                .map(|p| p.pty.bell_handle())?;
            let alive = sp.panes.iter().map(|p| p.pty.alive_handle()).collect();
            Some(Watched {
                bell,
                alive,
                label: label.to_string(),
            })
        });
        let mut shared = self.lock();
        match watched {
            Some(watched) => {
                shared.sessions.insert(key, watched);
            }
            None => {
                shared.sessions.remove(&key);
                shared.monitor.forget(dir);
                agent_state_store::clear(dir);
            }
        }
    }

    /// Kill and forget every live shell whose worktree lies at or under `root`.
    ///
    /// Called when a session is removed: deleting its worktree directory does not
    /// stop the shell (and any agent CLI) still running there, so without this the
    /// exited-looking-but-alive shell lingers in the pool keyed by its path. A
    /// session later recreated at the same path would then re-attach to that stale
    /// shell — inheriting the previous run's agent and scrollback — instead of
    /// spawning fresh. Dropping each [`PtySession`] kills its shell (via `Drop`),
    /// and the watched / monitor / phase state for the path is cleared too.
    pub fn remove_under(&mut self, root: &Path) {
        let removed: Vec<PathBuf> = self
            .sessions
            .keys()
            .filter(|path| path.as_path() == root || path.starts_with(root))
            .cloned()
            .collect();
        if removed.is_empty() {
            return;
        }
        for path in &removed {
            // Dropping the PtySession kills the shell it owns.
            self.sessions.remove(path);
        }
        let mut shared = self.lock();
        for path in &removed {
            shared.sessions.remove(path);
            shared.monitor.forget(path);
            agent_state_store::clear(path);
        }
    }

    /// Snapshot the live terminal for the session rooted at `dir`, resized to the
    /// current pane geometry, for the sidebar's read-only preview. Returns `None`
    /// when no live session is rooted there, so the right pane falls back to the
    /// command log. Resizing here keeps a backgrounded session's screen reflowed
    /// to the visible pane, exactly as attaching to it would.
    pub fn snapshot(&mut self, term: &Term, dir: &Path) -> Option<TerminalView> {
        let sp = self.sessions.get_mut(dir)?;
        if sp.panes.is_empty() {
            return None;
        }
        let active = sp.active.min(sp.panes.len() - 1);
        let session = &mut sp.panes[active].pty;
        if !session.is_alive() {
            return None;
        }
        let (height, width) = term.size();
        // The preview has no tab strip, so it uses the full-pane geometry.
        let geo = ui::terminal_geometry(height as usize, width as usize);
        session.resize(geo.rows, geo.cols);
        Some(TerminalView::from_screen(session.parser().screen()))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().expect("terminal monitor mutex poisoned")
    }
}

impl Drop for TerminalPool {
    fn drop(&mut self) {
        // Stop the watcher and wait for it, then the owned PTYs drop and kill
        // their shells.
        self.stop.store(true, Ordering::SeqCst);
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
    }
}

/// The pane kind a first launch opens: an agent CLI, or a plain terminal.
fn pane_kind(agent: bool) -> PaneKind {
    if agent {
        PaneKind::Agent
    } else {
        PaneKind::Terminal
    }
}

/// Spawn the watcher thread: every [`POLL_INTERVAL`] it prunes exited sessions,
/// feeds the live bell counts and recorded phases to the [`SessionMonitor`], and
/// fires a one-shot notification for each background session that has just begun
/// waiting for input or whose agent has just finished.
fn spawn_watcher(
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    notifications_enabled: bool,
) -> JoinHandle<()> {
    // One reader for the watcher's lifetime so its mtime cache survives across
    // ticks: an unchanged phase file then costs a single `stat`, not a re-read.
    let phase_reader = agent_state_store::PhaseReader::new();
    std::thread::spawn(move || loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(POLL_INTERVAL);

        // Snapshot the bookkeeping under the lock: prune dead sessions and observe
        // the phases/bells.
        let notices: Vec<(String, session_monitor::NoticeKind)> = {
            let mut shared = match shared.lock() {
                Ok(shared) => shared,
                Err(_) => break,
            };

            // Prune sessions whose every pane has exited so they stop being
            // tracked (the path is live while any pane is alive).
            let dead: Vec<PathBuf> = shared
                .sessions
                .iter()
                .filter(|(_, w)| !w.any_alive())
                .map(|(path, _)| path.clone())
                .collect();
            for path in dead {
                shared.sessions.remove(&path);
                shared.monitor.forget(&path);
                agent_state_store::clear(&path);
            }

            // Each session's reading pairs its bell count with the phase its
            // agent's hooks last recorded (if any); the monitor prefers the phase
            // and falls back to the bell.
            let readings: Vec<session_monitor::Reading> = shared
                .sessions
                .iter()
                .map(|(path, w)| {
                    (
                        path.clone(),
                        w.bell.load(Ordering::SeqCst),
                        phase_reader.read(path),
                    )
                })
                .collect();
            shared
                .monitor
                .observe(&readings)
                .into_iter()
                .filter_map(|notice| {
                    shared
                        .sessions
                        .get(&notice.path)
                        .map(|w| (w.label.clone(), notice.kind))
                })
                .collect()
        };

        if notifications_enabled {
            for (label, kind) in notices {
                notify(&label, kind);
            }
        }
    })
}

/// Show a desktop notification that a background session changed state: it began
/// waiting for input, or its agent finished.
///
/// Best-effort: failures (e.g. a headless environment without a notification
/// daemon) are ignored so they never disturb the watcher loop.
///
/// The body leads with a small ASCII-art rabbit rather than an emoji so it
/// renders consistently across notification daemons that lack emoji glyphs.
fn notify(label: &str, kind: session_monitor::NoticeKind) {
    let message = match kind {
        session_monitor::NoticeKind::Waiting => format!("{label} が入力待ちです"),
        session_monitor::NoticeKind::Done => format!("{label} が完了しました"),
    };
    let _ = notify_rust::Notification::new()
        .summary("usagi")
        .body(&format!("(\\_/)\n(='.'=)\n{message}"))
        .show();
}

impl Default for TerminalPool {
    fn default() -> Self {
        Self::new(true)
    }
}

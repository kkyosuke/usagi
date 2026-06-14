//! Keeps embedded terminal sessions alive in the background and watches them.
//!
//! The workspace screen can attach to a worktree's live shell (and an agent CLI
//! inside it). Unlike a one-shot terminal, these sessions persist after the user
//! detaches: [`TerminalManager`] owns one [`PtySession`] per worktree path, so a
//! background agent keeps running — and keeps producing output — while the user
//! works elsewhere in the screen.
//!
//! A background watcher thread polls every session's bell count through a
//! [`SessionMonitor`] (see [`crate::infrastructure::session_monitor`]). When a
//! detached session's agent rings the bell to ask for input, the watcher fires a
//! one-shot desktop notification and flags the session as waiting; the flag is
//! exposed through a [`MonitorHandle`] the presentation layer reads to mark the
//! session in the sidebar. Sessions whose shell has exited are pruned.
//!
//! This module is pure I/O and threading (PTYs, a watcher thread, desktop
//! notifications), so it is excluded from coverage — like [`crate::infrastructure::pty`].
//! Its pure core, the waiting-state bookkeeping, lives in [`SessionMonitor`] and
//! is tested there.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Result;

use crate::infrastructure::pty::PtySession;
use crate::infrastructure::session_monitor::SessionMonitor;

/// How often the watcher thread samples every session's bell count.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// The handles a background session is watched through, kept separate from the
/// owned [`PtySession`] so the watcher thread can poll without holding it.
struct Watched {
    /// The session's running bell count (shared with its [`PtySession`]).
    bell: Arc<AtomicU64>,
    /// The session's liveness flag; once `false`, the shell has exited.
    alive: Arc<AtomicBool>,
    /// A human label (the worktree branch) shown in the notification.
    label: String,
}

/// State shared between the manager, the watcher thread, and the UI.
#[derive(Default)]
struct Shared {
    monitor: SessionMonitor,
    sessions: HashMap<PathBuf, Watched>,
}

/// A cloneable read/notify handle onto the shared waiting state, given to the
/// presentation layer so its render loops can mark waiting sessions and declare
/// which session is in the foreground.
#[derive(Clone)]
pub struct MonitorHandle {
    shared: Arc<Mutex<Shared>>,
}

impl MonitorHandle {
    /// A handle backed by empty state and no watcher — for screens (and tests)
    /// that render without any live sessions.
    pub fn detached() -> Self {
        Self {
            shared: Arc::new(Mutex::new(Shared::default())),
        }
    }

    /// A snapshot of the worktree paths currently waiting for the user.
    pub fn waiting(&self) -> HashSet<PathBuf> {
        self.lock().monitor.waiting().clone()
    }

    /// Declare the foreground (attached) session, or clear it with `None`. The
    /// attached session is never reported as waiting.
    pub fn set_attached(&self, path: Option<PathBuf>) {
        self.lock().monitor.set_attached(path);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().expect("terminal monitor mutex poisoned")
    }
}

/// Owns the workspace screen's background terminal sessions and the watcher
/// thread monitoring them.
pub struct TerminalManager {
    /// Live shells, one per worktree path, borrowed by the attach loop.
    ptys: HashMap<PathBuf, PtySession>,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    watcher: Option<JoinHandle<()>>,
}

impl TerminalManager {
    /// Build a manager and start its watcher thread. `notifications_enabled`
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
            ptys: HashMap::new(),
            shared,
            stop,
            watcher: Some(watcher),
        }
    }

    /// A handle the presentation layer reads to render waiting markers and to
    /// declare the foreground session.
    pub fn handle(&self) -> MonitorHandle {
        MonitorHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Attach to the worktree's existing background session, spawning one (sized
    /// `rows`×`cols`, with `initial` sent to the shell on start) if none is
    /// running. Returns a mutable borrow of the live shell for the attach loop
    /// to drive.
    pub fn attach_or_spawn(
        &mut self,
        dir: &Path,
        rows: u16,
        cols: u16,
        initial: Option<&str>,
        label: &str,
    ) -> Result<&mut PtySession> {
        if !self.ptys.contains_key(dir) {
            let mut pty = PtySession::spawn(dir, rows, cols)?;
            if let Some(command) = initial {
                pty.write(format!("{command}\r").as_bytes())?;
            }
            self.lock().sessions.insert(
                dir.to_path_buf(),
                Watched {
                    bell: pty.bell_handle(),
                    alive: pty.alive_handle(),
                    label: label.to_string(),
                },
            );
            self.ptys.insert(dir.to_path_buf(), pty);
        }
        Ok(self
            .ptys
            .get_mut(dir)
            .expect("session was just spawned or already present"))
    }

    /// Forget the worktree's session if its shell has exited, so it stops being
    /// tracked and a later attach spawns a fresh one.
    pub fn reap_if_dead(&mut self, dir: &Path) {
        let dead = self.ptys.get(dir).map(|p| !p.is_alive()).unwrap_or(false);
        if dead {
            self.ptys.remove(dir);
            let mut shared = self.lock();
            shared.sessions.remove(dir);
            shared.monitor.forget(dir);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().expect("terminal monitor mutex poisoned")
    }
}

impl Drop for TerminalManager {
    fn drop(&mut self) {
        // Stop the watcher and wait for it, then the owned PTYs drop and kill
        // their shells.
        self.stop.store(true, Ordering::SeqCst);
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
    }
}

/// Spawn the watcher thread: every [`POLL_INTERVAL`] it prunes exited sessions,
/// feeds the live bell counts to the [`SessionMonitor`], and fires a one-shot
/// notification for each session that has just begun waiting for input.
fn spawn_watcher(
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    notifications_enabled: bool,
) -> JoinHandle<()> {
    std::thread::spawn(move || loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(POLL_INTERVAL);

        let labels: Vec<String> = {
            let mut shared = match shared.lock() {
                Ok(shared) => shared,
                Err(_) => break,
            };

            // Prune sessions whose shell has exited so they stop being tracked.
            let dead: Vec<PathBuf> = shared
                .sessions
                .iter()
                .filter(|(_, w)| !w.alive.load(Ordering::SeqCst))
                .map(|(path, _)| path.clone())
                .collect();
            for path in dead {
                shared.sessions.remove(&path);
                shared.monitor.forget(&path);
            }

            let readings: Vec<(PathBuf, u64)> = shared
                .sessions
                .iter()
                .map(|(path, w)| (path.clone(), w.bell.load(Ordering::SeqCst)))
                .collect();
            let newly = shared.monitor.observe(&readings);
            newly
                .iter()
                .filter_map(|path| shared.sessions.get(path).map(|w| w.label.clone()))
                .collect()
        };

        if notifications_enabled {
            for label in labels {
                notify_waiting(&label);
            }
        }
    })
}

/// Show a desktop notification that a background session is waiting for input.
///
/// Best-effort: failures (e.g. a headless environment without a notification
/// daemon) are ignored so they never disturb the watcher loop, exactly as
/// `hop`'s welcome notification does.
fn notify_waiting(label: &str) {
    let _ = notify_rust::Notification::new()
        .summary("usagi")
        .body(&format!("🐰 {label} が入力待ちです"))
        .show();
}

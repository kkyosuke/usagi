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

use super::terminal_view::TerminalView;
use super::ui;

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
                    alive: Arc::new(AtomicBool::new(true)),
                    label: String::new(),
                },
            );
        }
        Self {
            shared: Arc::new(Mutex::new(shared)),
        }
    }

    /// A snapshot of the worktree paths currently waiting for the user.
    pub fn waiting(&self) -> HashSet<PathBuf> {
        self.lock().monitor.waiting().clone()
    }

    /// A snapshot of the worktree paths whose agent has finished (exited).
    pub fn done(&self) -> HashSet<PathBuf> {
        self.lock().monitor.done().clone()
    }

    /// A snapshot of the worktree paths with a live (running) embedded session:
    /// a shell — and any agent CLI inside it — is still alive, whether attached
    /// or left running in the background. The render loops read this to mark
    /// sessions that have an agent in use.
    pub fn live(&self) -> HashSet<PathBuf> {
        let shared = self.lock();
        shared
            .sessions
            .iter()
            .filter(|(_, w)| w.alive.load(Ordering::SeqCst))
            .map(|(path, _)| path.clone())
            .collect()
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

/// The live shells embedded in the workspace screen, keyed by worktree path.
///
/// Owned by the screen ([`super::run`]); dropped when the user leaves it, which
/// kills every shell it still holds (via [`PtySession`]'s `Drop`) and stops the
/// watcher thread.
pub struct TerminalPool {
    sessions: HashMap<PathBuf, PtySession>,
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

    /// Borrow the live shell rooted at `dir`, spawning one if none exists yet
    /// (or the previous one has exited). On a fresh spawn the `initial` command
    /// line is sent once — this is how `agent` lands the user in the configured
    /// agent CLI; re-attaching to an existing shell never re-sends it. `label`
    /// (the worktree branch) is shown in the waiting notification.
    ///
    /// The shell is sized to the right pane's current geometry; the terminal
    /// loop resizes it from then on as the window changes.
    pub fn attach_or_spawn(
        &mut self,
        term: &Term,
        dir: &Path,
        initial: Option<&str>,
        label: &str,
    ) -> Result<&mut PtySession> {
        let key = dir.to_path_buf();
        let alive = self.sessions.get(&key).is_some_and(|s| s.is_alive());
        if !alive {
            let (height, width) = term.size();
            let geo = ui::terminal_geometry(height as usize, width as usize);
            // The launch command is handed to the shell as an argument (not typed
            // into its stdin) so the shell never echoes the long line into the
            // pane before the agent draws over it — see [`PtySession::spawn`].
            let pty = PtySession::spawn(dir, geo.rows, geo.cols, initial)?;
            // Register (or refresh) the watched handles for this path; a fresh
            // spawn over an exited one resets its bell baseline.
            // Forget any phase recorded by a previous agent at this worktree so
            // the fresh session does not inherit a stale running / waiting state
            // before its own hooks fire.
            agent_state_store::clear(dir);
            {
                let mut shared = self.lock();
                shared.monitor.forget(dir);
                shared.sessions.insert(
                    key.clone(),
                    Watched {
                        bell: pty.bell_handle(),
                        alive: pty.alive_handle(),
                        label: label.to_string(),
                    },
                );
            }
            // Overwrites (and so drops/kills) any exited shell at this path.
            self.sessions.insert(key.clone(), pty);
        }
        Ok(self
            .sessions
            .get_mut(&key)
            .expect("the session was just inserted or already present"))
    }

    /// Snapshot the live terminal for the session rooted at `dir`, resized to the
    /// current pane geometry, for the sidebar's read-only preview. Returns `None`
    /// when no live session is rooted there, so the right pane falls back to the
    /// command log. Resizing here keeps a backgrounded session's screen reflowed
    /// to the visible pane, exactly as attaching to it would.
    pub fn snapshot(&mut self, term: &Term, dir: &Path) -> Option<TerminalView> {
        let session = self.sessions.get_mut(dir)?;
        if !session.is_alive() {
            return None;
        }
        let (height, width) = term.size();
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

/// Spawn the watcher thread: every [`POLL_INTERVAL`] it prunes exited sessions,
/// feeds the live bell counts to the [`SessionMonitor`], and fires a one-shot
/// notification for each background session that has just begun waiting for input
/// or whose agent has just finished.
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

        let notices: Vec<(String, session_monitor::NoticeKind)> = {
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
                        agent_state_store::read(path),
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

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
//! The geometry it spawns at ([`super::super::ui::terminal_geometry`]) is tested on its
//! own.

use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Result;
use console::Term;

use crate::domain::resource::{aggregate_by_root, ResourceUsage};
use crate::domain::settings::{AgentCli, Sidebar};
use crate::domain::workspace_state::PrLink;
use crate::infrastructure::open_panes_store::{StoredPane, StoredPaneKind};
use crate::infrastructure::pty::{PtyInputHandle, PtySession, ScreenCallbacks};
use crate::infrastructure::resource::{ResourceSampler, SysinfoSampler};
use crate::infrastructure::session_monitor::SessionMonitor;
use crate::infrastructure::{
    agent_live_prompt_store, agent_state_store, error_log, session_monitor,
};

use super::super::pane_input;
use super::super::ui;
use super::tabs::{self, PaneKind, PaneTab, TabNav, TabSwap};
use super::view::TerminalView;

/// How often the watcher thread samples every session's bell count.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// How many [`POLL_INTERVAL`] bell ticks pass between resource (CPU / memory)
/// samples. Reading every process is far heavier than reading a bell counter, and
/// CPU use is meaningful only over a window — so it is sampled on a slower beat
/// (every tenth tick ≈ two seconds) rather than on every poll. The sidebar's
/// figures are coarse health indicators, not a profiler, so this halves the
/// full-system process-table refresh cost while keeping the display fresh enough.
const RESOURCE_SAMPLE_EVERY: u32 = 10;

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
    /// The root process id of each live pane's shell — also its process-group id
    /// (portable-pty makes the shell a session leader), so the resource sampler
    /// totals each shell's whole subtree (the shell and any agent CLI beneath it).
    roots: Vec<u32>,
    /// Every live pane's parser/generation handles, so the watcher can harvest
    /// pull-request URLs from panes left running in the background. The render
    /// loop already does this for the attached pane; keeping the handles here
    /// makes detached panes update the sidebar without waiting for a later
    /// workspace re-sync.
    pr_panes: Vec<WatchedPrPane>,
    /// Input handle for the agent pane, when this session currently has one.
    /// The watcher drains MCP live `session_prompt` prompts only for sessions with
    /// this handle, so prompts sent while no agent pane is live remain queued.
    agent_input: Option<PtyInputHandle>,
    /// A human label (the worktree branch) shown in the notification.
    label: String,
}

impl Watched {
    /// Whether the session still has at least one live pane.
    fn any_alive(&self) -> bool {
        self.alive.iter().any(|a| a.load(Ordering::SeqCst))
    }
}

/// The cheap shared handles the watcher needs to harvest PR URLs from one pane.
///
/// `last_generation` and `last_prs` are watcher-owned cache fields: they avoid
/// rescanning unchanged screens and avoid re-writing the same visible PR list to
/// the store every tick. The pane `id` is stable across tab reorders, so a scan
/// job can be matched back to its cache after it runs off-lock.
struct WatchedPrPane {
    id: u64,
    parser: Arc<Mutex<vt100::Parser<ScreenCallbacks>>>,
    generation: Arc<AtomicU64>,
    last_generation: u64,
    last_prs: Vec<PrLink>,
}

/// A live agent input target snapshotted out of [`Shared`] so the watcher can
/// release the shared-state lock before it drains disk queues or writes to PTYs.
struct LivePromptTarget {
    path: PathBuf,
    input: PtyInputHandle,
}

/// State shared between the pool, the watcher thread, and the render loops.
#[derive(Default)]
struct Shared {
    monitor: SessionMonitor,
    sessions: HashMap<PathBuf, Watched>,
    /// PR lists the watcher has newly harvested from live panes, keyed by the
    /// session/worktree root. The event loop drains this and calls
    /// `HomeState::set_pr_links`, so background sessions get their sidebar `#N`
    /// badges as soon as the URL appears in their output.
    pr_link_updates: HashMap<PathBuf, Vec<PrLink>>,
    /// The CPU / memory each live session is using, keyed by worktree path, as of
    /// the watcher's last resource sample. Empty while nothing is live (the
    /// watcher skips sampling then), so an idle workspace carries no figures.
    resources: HashMap<PathBuf, ResourceUsage>,
    /// The workspace total — the sum across every live session's process tree —
    /// as of the last sample. Idle (zero) while nothing is live.
    resource_total: ResourceUsage,
}

/// A cloneable read/notify handle onto the shared waiting state, given to the
/// render loops so they can mark waiting sessions and declare which session is
/// in the foreground.
#[derive(Clone)]
pub struct MonitorHandle {
    shared: Arc<Mutex<Shared>>,
    /// A monotonic counter the watcher (and pane registration) bump whenever the
    /// badge sets behind [`snapshot`](Self::snapshot) could have moved. The render
    /// loops read it lock-free each frame and only take the lock to re-snapshot
    /// (and clone the sets) when it advances, so an unchanged frame costs a single
    /// atomic load instead of cloning every badge set. Monotonic, so a missed bump
    /// only ever causes one extra (harmless) re-snapshot, never a stale one.
    version: Arc<AtomicU64>,
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
    /// The CPU / memory each live session is using, keyed by worktree path, from
    /// the watcher's last resource sample. A session with no entry (not yet
    /// sampled, or not live) shows no figure.
    pub resources: HashMap<PathBuf, ResourceUsage>,
    /// The workspace total across every live session's process tree, from the
    /// last sample — idle (zero) while nothing is live, so the sidebar omits it.
    pub resource_total: ResourceUsage,
}

impl MonitorHandle {
    /// A handle backed by empty state and no watcher — for screens (and tests)
    /// that render without any live sessions.
    pub fn detached() -> Self {
        Self {
            shared: Arc::new(Mutex::new(Shared::default())),
            version: Arc::new(AtomicU64::new(0)),
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
                    roots: Vec::new(),
                    pr_panes: Vec::new(),
                    agent_input: None,
                    label: String::new(),
                },
            );
        }
        Self {
            shared: Arc::new(Mutex::new(shared)),
            version: Arc::new(AtomicU64::new(1)),
        }
    }

    /// A handle reporting the given paths as waiting for input, for tests that
    /// exercise the waiting-first sort without driving a real agent (the waiting
    /// state is seeded through the monitor exactly as a reported `Waiting` phase
    /// would).
    #[cfg(test)]
    pub fn with_waiting(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        use crate::domain::agent_phase::AgentPhase;
        let mut shared = Shared::default();
        let readings: Vec<crate::infrastructure::session_monitor::Reading> = paths
            .into_iter()
            .map(|path| (path, 0, Some(AgentPhase::Waiting)))
            .collect();
        shared.monitor.observe(&readings);
        Self {
            shared: Arc::new(Mutex::new(shared)),
            version: Arc::new(AtomicU64::new(1)),
        }
    }

    /// A handle seeded with PR updates, for exercising the event-loop drain path
    /// without spawning a real watcher thread.
    #[cfg(test)]
    pub fn with_pr_link_updates(updates: impl IntoIterator<Item = (PathBuf, Vec<PrLink>)>) -> Self {
        let shared = Shared {
            pr_link_updates: updates.into_iter().collect(),
            ..Shared::default()
        };
        Self {
            shared: Arc::new(Mutex::new(shared)),
            version: Arc::new(AtomicU64::new(1)),
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
        snapshot_locked(&shared)
    }

    /// The current badge version (see [`version`](Self::version)). A render loop
    /// caches the value it last snapshotted at and only re-snapshots when this
    /// differs, skipping the per-frame clone of every badge set when nothing moved.
    pub fn badge_version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    /// Drain the PR-link updates harvested by the watcher since the previous
    /// call. Each entry is the store's accumulated, de-duplicated list for a
    /// session root, ready for `HomeState::set_pr_links`.
    pub fn take_pr_link_updates(&self) -> HashMap<PathBuf, Vec<PrLink>> {
        std::mem::take(&mut self.lock().pr_link_updates)
    }

    /// Declare the foreground (attached) session, or clear it with `None`. The
    /// attached session is shown with its true state like any other; being
    /// attached only suppresses its desktop notification and the bell heuristic.
    pub fn set_attached(&self, path: Option<PathBuf>) {
        self.lock().monitor.set_attached(path);
    }

    /// Recovers the guard rather than panicking if the lock was poisoned: this
    /// runs on the render path (`snapshot` / `set_attached`), and any thread that
    /// panicked while holding `Shared` would poison the mutex, so an `expect` here
    /// would escalate it into a crash of the whole TUI — leaving the terminal in
    /// raw mode. A possibly-stale badge snapshot beats taking the UI down. Mirrors
    /// the watcher thread's poison handling and [`PtySession::parser`].
    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Build the renderable badge snapshot from already-locked watcher state. Kept
/// as a helper so the hot render path and the watcher can compare before/after
/// states without duplicating the clone/derive logic.
fn snapshot_locked(shared: &Shared) -> MonitorSnapshot {
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
        resources: shared.resources.clone(),
        resource_total: shared.resource_total,
    }
}

/// One off-lock PR scan the watcher should perform for a pane whose output
/// generation advanced.
struct PrScanJob {
    path: PathBuf,
    pane_id: u64,
    parser: Arc<Mutex<vt100::Parser<ScreenCallbacks>>>,
    previous: Vec<PrLink>,
}

/// The harvested result of one [`PrScanJob`].
struct PrScanResult {
    path: PathBuf,
    pane_id: u64,
    prs: Vec<PrLink>,
}

/// Collect the panes whose output changed since their last watcher scan, and
/// mark their current generation as observed. The actual parser locks and disk
/// writes happen after the shared watcher mutex is released.
fn pending_pr_scans(shared: &mut Shared) -> Vec<PrScanJob> {
    shared
        .sessions
        .iter_mut()
        .flat_map(|(path, watched)| {
            watched.pr_panes.iter_mut().filter_map(|pane| {
                let generation = pane.generation.load(Ordering::SeqCst);
                (pane.last_generation != generation).then(|| {
                    pane.last_generation = generation;
                    PrScanJob {
                        path: path.clone(),
                        pane_id: pane.id,
                        parser: Arc::clone(&pane.parser),
                        previous: pane.last_prs.clone(),
                    }
                })
            })
        })
        .collect()
}

/// Run PR scans off the watcher mutex. A pane with no visible PRs, or the same
/// visible PR list as its previous scan, yields no result.
fn scan_pr_jobs(jobs: Vec<PrScanJob>) -> Vec<PrScanResult> {
    jobs.into_iter()
        .filter_map(|job| {
            let prs = {
                let parser = lock_parser(&job.parser);
                super::link::pr_links(parser.screen())
            };
            (!prs.is_empty() && prs != job.previous).then_some(PrScanResult {
                path: job.path,
                pane_id: job.pane_id,
                prs,
            })
        })
        .collect()
}

fn lock_parser(
    parser: &Arc<Mutex<vt100::Parser<ScreenCallbacks>>>,
) -> MutexGuard<'_, vt100::Parser<ScreenCallbacks>> {
    parser
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Persist newly harvested PRs and return the store's accumulated list for each
/// session root whose visible PRs changed. Disk IO is kept out of the watcher
/// mutex; failures are best-effort like the attached-pane harvest path.
fn persist_pr_results(results: &[PrScanResult]) -> Vec<(PathBuf, Vec<PrLink>)> {
    results
        .iter()
        .map(|result| {
            let _ = crate::infrastructure::pr_link_store::add(&result.path, &result.prs);
            let merged = crate::infrastructure::pr_link_store::get(&result.path);
            (result.path.clone(), merged)
        })
        .collect()
}

/// Update the watcher's per-pane PR cache, queue sidebar updates for the event
/// loop, and return whether any queued list changed.
fn apply_pr_results(
    shared: &mut Shared,
    results: Vec<PrScanResult>,
    merged: Vec<(PathBuf, Vec<PrLink>)>,
) -> bool {
    for result in results {
        if let Some(pane) = shared.sessions.get_mut(&result.path).and_then(|watched| {
            watched
                .pr_panes
                .iter_mut()
                .find(|pane| pane.id == result.pane_id)
        }) {
            pane.last_prs = result.prs;
        }
    }

    let mut changed = false;
    for (path, prs) in merged {
        if shared.pr_link_updates.get(&path) != Some(&prs) {
            shared.pr_link_updates.insert(path, prs);
            changed = true;
        }
    }
    changed
}

/// One embedded pane: a live [`PtySession`] and what it runs (so the tab strip
/// can label it and the agent pane can be told apart for the badge heuristic).
struct Pane {
    /// Stable creation id used to number duplicate tab labels independently of
    /// the current tab-strip order.
    id: u64,
    pty: PtySession,
    kind: PaneKind,
    label_override: Option<String>,
    /// For an agent pane, which CLI it ran — recorded so the open-panes snapshot
    /// can restore the same agent (and resume it) on the next startup. `None` for
    /// a terminal pane.
    cli: Option<AgentCli>,
}

/// The panes of one session (worktree), in tab order, and which one is active
/// (visible / driven). A session keeps every pane alive in the background; only
/// the active one is attached at a time.
struct SessionPanes {
    panes: Vec<Pane>,
    active: usize,
    /// Cached labels for `panes`, rebuilt only when panes are added/closed. The
    /// active index changes far more often (and previews read tabs every frame),
    /// but it does not affect labels.
    tab_labels: Vec<String>,
}

impl SessionPanes {
    fn new(panes: Vec<Pane>, active: usize) -> Self {
        let mut this = Self {
            panes,
            active,
            tab_labels: Vec::new(),
        };
        this.rebuild_tab_labels();
        this
    }

    fn rebuild_tab_labels(&mut self) {
        let tabs: Vec<PaneTab> = self
            .panes
            .iter()
            .map(|p| PaneTab {
                kind: p.kind,
                cli: p.cli,
                id: p.id,
            })
            .collect();
        // Start from the generated `agent` / `terminal N` labels, then let any
        // per-pane rename override win (an empty override falls back to default).
        self.tab_labels = tabs::tab_labels(&tabs)
            .into_iter()
            .zip(self.panes.iter())
            .map(|(default, pane)| {
                pane.label_override
                    .as_deref()
                    .filter(|label| !label.trim().is_empty())
                    .unwrap_or(&default)
                    .to_string()
            })
            .collect();
    }
}

/// The per-spawn inputs that travel together whenever the pool creates a pane.
/// Bundling them keeps the public pool methods small while making it explicit
/// that the launch command, agent CLI metadata, notification label, and child
/// environment all describe the same pane spawn.
#[derive(Clone, Copy)]
pub struct PaneLaunch<'a> {
    pub agent_command: Option<&'a str>,
    pub cli: AgentCli,
    pub label: &'a str,
    pub env: &'a BTreeMap<String, String>,
}

/// The live shells embedded in the workspace screen, keyed by worktree path —
/// each path holding one or more panes (an agent alongside any terminals).
///
/// Owned by the screen ([`super::super::run`]); dropped when the user leaves it, which
/// kills every shell it still holds (via [`PtySession`]'s `Drop`) and stops the
/// watcher thread.
pub struct TerminalPool {
    sessions: HashMap<PathBuf, SessionPanes>,
    shared: Arc<Mutex<Shared>>,
    /// Monotonic badge-state version shared with [`MonitorHandle`]. Bumped by
    /// session registration/removal and by the watcher when monitor/resource
    /// state changes, letting render loops avoid cloning unchanged snapshots.
    version: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    watcher: Option<JoinHandle<()>>,
    /// Set true whenever a session is registered, and cleared by the watcher (under
    /// the shared lock) on the tick it observes no sessions. The watcher reads it
    /// *without* taking the lock, so an idle workspace with no live session stops
    /// locking `Shared` and walking an empty map five times a second — it just
    /// loads this flag and sleeps. Only ever set true here; the watcher is the sole
    /// authority that clears it, so a removal need not touch it (the next locked
    /// tick observes the empty map and clears it itself).
    has_sessions: Arc<AtomicBool>,
    /// The last 切替 preview built by [`snapshot`](TerminalPool::snapshot), so a
    /// frame whose previewed session, geometry, and output are all unchanged
    /// returns the cached view without re-resizing or re-snapshotting the grid.
    preview_cache: Option<PreviewCache>,
    /// How many scrolled-off lines each spawned pane keeps — the configured
    /// [`Settings::terminal_scrollback_lines`](crate::domain::settings::Settings),
    /// handed to every [`PtySession::spawn`].
    scrollback_lines: usize,
    /// Monotonic id source for panes spawned during this TUI run. The id is not a
    /// storage key; it exists only to keep duplicate tab labels stable while the
    /// user reorders tabs.
    next_pane_id: u64,
}

/// The previewed session and the inputs the last [`TerminalView`] was built
/// from, so [`snapshot`](TerminalPool::snapshot) can skip the `resize` ioctl when
/// the geometry is unchanged and the `from_screen` rebuild when the output is.
struct PreviewCache {
    dir: PathBuf,
    /// The session's active pane index the cached view was snapshotted from. A
    /// session has one generation counter *per pane* (each starts at 0), so two
    /// quiet panes can share a generation value; without this, switching the
    /// active tab while `dir` and `geo` are unchanged could return the previously
    /// active pane's view for the now-active tab.
    active: usize,
    geo: ui::TerminalGeometry,
    generation: u64,
    view: TerminalView,
}

impl TerminalPool {
    /// An empty pool with its watcher thread running. `notifications_enabled`
    /// gates the desktop notification fired when a detached session starts
    /// waiting for input. `scrollback_lines` is how much scrollback each spawned
    /// pane keeps (the configured
    /// [`Settings::terminal_scrollback_lines`](crate::domain::settings::Settings)).
    pub fn new(notifications_enabled: bool, scrollback_lines: usize) -> Self {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let version = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        // Starts empty, so the watcher idles without locking until the first
        // session is registered (see [`has_sessions`](Self::has_sessions)).
        let has_sessions = Arc::new(AtomicBool::new(false));
        let watcher = spawn_watcher(
            Arc::clone(&shared),
            Arc::clone(&version),
            Arc::clone(&stop),
            Arc::clone(&has_sessions),
            notifications_enabled,
            Box::new(SysinfoSampler::new()),
        );
        Self {
            sessions: HashMap::new(),
            shared,
            version,
            stop,
            watcher: Some(watcher),
            has_sessions,
            preview_cache: None,
            scrollback_lines,
            next_pane_id: 0,
        }
    }

    /// A handle the render loops read to mark waiting sessions and to declare
    /// the foreground session.
    pub fn monitor(&self) -> MonitorHandle {
        MonitorHandle {
            shared: Arc::clone(&self.shared),
            version: Arc::clone(&self.version),
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
        launch: PaneLaunch<'_>,
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
            let kind = tabs::pane_kind(agent);
            let pane = self.spawn_pane(term, dir, kind, launch)?;
            self.sessions.insert(key, SessionPanes::new(vec![pane], 0));
        }
        self.refresh_watched(dir, launch.label);
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
        launch: PaneLaunch<'_>,
    ) -> Result<()> {
        let pane = self.spawn_pane(term, dir, kind, launch)?;
        let sp = self
            .sessions
            .entry(dir.to_path_buf())
            .or_insert_with(|| SessionPanes::new(Vec::new(), 0));
        sp.panes.push(pane);
        sp.active = sp.panes.len().saturating_sub(1);
        sp.rebuild_tab_labels();
        self.refresh_watched(dir, launch.label);
        Ok(())
    }

    /// Spawn a new pane of `kind` for `dir` **without** making it active — the
    /// background-tab path (在席's `terminal` / `agent` when the session already
    /// shows tabs). The new tab appears in the strip while the previously active
    /// pane stays attached; the caller switches to it (via [`activate_pane_id`])
    /// only once it is ready and the user has not acted in the meantime. Returns
    /// the new pane's stable id so the caller can track / activate exactly this
    /// pane. An agent pane sends `agent_command` once on spawn; a terminal pane
    /// opens a plain shell.
    ///
    /// [`activate_pane_id`]: Self::activate_pane_id
    pub fn add_pane_inactive(
        &mut self,
        term: &Term,
        dir: &Path,
        kind: PaneKind,
        launch: PaneLaunch<'_>,
    ) -> Result<u64> {
        let pane = self.spawn_pane(term, dir, kind, launch)?;
        let id = pane.id;
        let sp = self
            .sessions
            .entry(dir.to_path_buf())
            .or_insert_with(|| SessionPanes::new(Vec::new(), 0));
        sp.panes.push(pane);
        // Deliberately leave `active` where it is (unlike `add_pane`): the new
        // pane loads in the background while the current tab stays attached.
        sp.rebuild_tab_labels();
        self.refresh_watched(dir, launch.label);
        Ok(id)
    }

    /// Make the pane with stable `id` the active tab for `dir`, returning whether
    /// a pane with that id was found. The background-tab counterpart to
    /// [`add_pane_inactive`]: once the loading pane is ready (and the user has not
    /// acted) the caller activates it, then re-attaches the now-active pane.
    pub fn activate_pane_id(&mut self, dir: &Path, id: u64) -> bool {
        match self.sessions.get_mut(dir) {
            Some(sp) => match sp.panes.iter().position(|p| p.id == id) {
                Some(idx) => {
                    sp.active = idx;
                    true
                }
                None => false,
            },
            None => false,
        }
    }

    /// The 0-based tab index of the pane with stable `id`, or `None` when no pane
    /// in `dir` carries it any more (it closed, or the session emptied). The
    /// renderer uses it to draw the loading animation on the right chip while a
    /// background pane starts, and the loop uses `None` to drop a pending tab that
    /// vanished before it was ready.
    pub fn tab_index_of(&self, dir: &Path, id: u64) -> Option<usize> {
        self.sessions
            .get(dir)?
            .panes
            .iter()
            .position(|p| p.id == id)
    }

    /// Whether the background pane with stable `id` has started painting — its
    /// shell produced at least one screen update (`generation > 0`) or has already
    /// exited (so a shell that dies on spawn stops the wait rather than hanging).
    /// `false` for a missing pane; the caller pairs this with [`tab_index_of`] to
    /// tell "still starting" from "gone".
    ///
    /// [`tab_index_of`]: Self::tab_index_of
    pub fn pane_ready(&self, dir: &Path, id: u64) -> bool {
        self.sessions
            .get(dir)
            .and_then(|sp| sp.panes.iter().find(|p| p.id == id))
            .is_some_and(|p| p.pty.generation() > 0 || !p.pty.is_alive())
    }

    /// Set `dir`'s active tab directly (clamped to the pane count), for restoring
    /// the tab that was active when the session's panes were last persisted. A
    /// no-op for a session with no panes.
    pub fn set_active(&mut self, dir: &Path, active: usize) {
        if let Some(sp) = self.sessions.get_mut(dir) {
            if !sp.panes.is_empty() {
                sp.active = active.min(sp.panes.len().saturating_sub(1));
            }
        }
    }

    /// Move the active tab within `dir` (next / previous / a numbered jump),
    /// leaving every pane alive. A no-op for a session with no panes. Returns
    /// whether the active tab actually moved, so 没入 can skip the screen clear /
    /// repaint when a nav lands on the tab already showing (a lone pane, or a
    /// jump to the current tab) and would otherwise flicker.
    pub fn nav(&mut self, dir: &Path, nav: TabNav) -> bool {
        match self.sessions.get_mut(dir) {
            Some(sp) => {
                let before = sp.active;
                sp.active = tabs::resolve_nav(sp.active, sp.panes.len(), nav);
                sp.active != before
            }
            None => false,
        }
    }

    /// Move the active tab one slot left / right, keeping the moved pane active.
    /// The move does not wrap around the ends. Returns whether the tab order
    /// changed so the caller can skip a flickering redraw on edge no-ops.
    pub fn swap_active(&mut self, dir: &Path, swap: TabSwap) -> bool {
        match self.sessions.get_mut(dir) {
            Some(sp) => match tabs::resolve_swap(sp.active, sp.panes.len(), swap) {
                Some((from, to)) => {
                    sp.panes.swap(from, to);
                    sp.active = to;
                    sp.rebuild_tab_labels();
                    true
                }
                None => false,
            },
            None => false,
        }
    }

    /// Move `from` tab to `target` for tab drag/drop. `target` is clamped to the
    /// last live tab, and the active cursor stays on whichever pane was active
    /// before the drag (unless that pane itself is the one moved).
    pub fn move_tab(&mut self, dir: &Path, from: usize, target: usize) -> bool {
        match self.sessions.get_mut(dir) {
            Some(sp) => match tabs::resolve_move(from, target, sp.panes.len()) {
                Some((from, to)) => {
                    let active = tabs::active_after_move(
                        sp.active.min(sp.panes.len().saturating_sub(1)),
                        from,
                        to,
                    );
                    let pane = sp.panes.remove(from);
                    sp.panes.insert(to, pane);
                    sp.active = active;
                    sp.rebuild_tab_labels();
                    true
                }
                None => false,
            },
            None => false,
        }
    }

    /// Move a concrete tab one slot left / right and keep that tab active.
    pub fn move_tab_by(&mut self, dir: &Path, tab: usize, swap: TabSwap) -> bool {
        match self.sessions.get_mut(dir) {
            Some(sp) => match tabs::resolve_swap(tab, sp.panes.len(), swap) {
                Some((from, to)) => {
                    sp.panes.swap(from, to);
                    sp.active = to;
                    sp.rebuild_tab_labels();
                    true
                }
                None => false,
            },
            None => false,
        }
    }

    /// Rename a concrete tab. An empty `label` clears the override and falls back
    /// to the generated `agent` / `terminal N` label.
    pub fn rename_tab(&mut self, dir: &Path, tab: usize, label: &str) -> bool {
        match self.sessions.get_mut(dir) {
            Some(sp) if tab < sp.panes.len() => {
                let trimmed = label.trim();
                sp.panes[tab].label_override = (!trimmed.is_empty()).then(|| trimmed.to_string());
                sp.rebuild_tab_labels();
                true
            }
            _ => false,
        }
    }

    /// Close a concrete tab, killing its shell. Returns whether any pane remains.
    pub fn close_tab(&mut self, dir: &Path, tab: usize, label: &str) -> bool {
        let key = dir.to_path_buf();
        let remains = match self.sessions.get_mut(&key) {
            Some(sp) if tab < sp.panes.len() => {
                let len_before = sp.panes.len();
                sp.panes.remove(tab);
                sp.rebuild_tab_labels();
                match tabs::active_after_close(sp.active.min(len_before - 1), len_before) {
                    Some(next) => {
                        // If a tab before the active one closed, the same pane shifts
                        // left; if the active tab closed, active_after_close picks the
                        // nearest successor/predecessor. If a tab after it closed, the
                        // active index stays put.
                        sp.active = if tab < sp.active {
                            sp.active.saturating_sub(1)
                        } else if tab == sp.active {
                            next
                        } else {
                            sp.active.min(sp.panes.len().saturating_sub(1))
                        };
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

    /// Close `dir`'s active pane, killing its shell (its [`PtySession`] drops).
    /// Returns whether any pane remains: `true` leaves the next tab active so the
    /// caller keeps driving, `false` means the session is empty and the caller
    /// drops back to 在席. The whole session entry is removed when it empties.
    pub fn close_active(&mut self, dir: &Path, label: &str) -> bool {
        let key = dir.to_path_buf();
        let remains = match self.sessions.get_mut(&key) {
            Some(sp) if !sp.panes.is_empty() => {
                let active = sp.active.min(sp.panes.len().saturating_sub(1));
                let len_before = sp.panes.len();
                // Dropping the removed Pane kills the shell it owns.
                sp.panes.remove(active);
                sp.rebuild_tab_labels();
                match tabs::active_after_close(active, len_before) {
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

    /// Whether `dir` already holds an agent pane running `cli`. A session keeps
    /// at most one agent *per CLI*, so a request to add another of the same CLI
    /// (在席's `agent`, or `Ctrl-G`) reads this to jump to the existing tab
    /// instead of spawning a duplicate — while a *different* CLI still opens a
    /// new agent pane alongside (see [`activate_agent_of`](Self::activate_agent_of)).
    pub fn has_agent_pane_of(&self, dir: &Path, cli: AgentCli) -> bool {
        self.sessions.get(dir).is_some_and(|sp| {
            sp.panes
                .iter()
                .any(|p| matches!(p.kind, PaneKind::Agent) && p.cli == Some(cli))
        })
    }

    /// Make `dir`'s agent pane running `cli` the active tab, returning whether
    /// one was found. Lets a request to add an agent of a CLI reuse the existing
    /// one — a session holds at most one agent per CLI — by activating its tab
    /// rather than spawning a duplicate.
    pub fn activate_agent_of(&mut self, dir: &Path, cli: AgentCli) -> bool {
        match self.sessions.get_mut(dir) {
            Some(sp) => match sp
                .panes
                .iter()
                .position(|p| matches!(p.kind, PaneKind::Agent) && p.cli == Some(cli))
            {
                Some(idx) => {
                    sp.active = idx;
                    true
                }
                None => false,
            },
            None => false,
        }
    }

    /// Borrow `dir`'s active pane's shell, or `None` when the session has no
    /// panes — the pane the terminal loop drives.
    pub fn active_pty(&mut self, dir: &Path) -> Option<&mut PtySession> {
        let sp = self.sessions.get_mut(dir)?;
        if sp.panes.is_empty() {
            return None;
        }
        let active = sp.active.min(sp.panes.len().saturating_sub(1));
        Some(&mut sp.panes[active].pty)
    }

    /// The tab strip for `dir`: a label per pane (in tab order) and the active
    /// index, for the renderer to draw above the embedded terminal. Empty when no
    /// session is rooted there.
    pub fn tabs(&self, dir: &Path) -> (Vec<String>, usize) {
        match self.sessions.get(dir) {
            Some(sp) => {
                let active = sp.active.min(sp.panes.len().saturating_sub(1));
                (sp.tab_labels.clone(), active)
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
        &mut self,
        term: &Term,
        dir: &Path,
        kind: PaneKind,
        launch: PaneLaunch<'_>,
    ) -> Result<Pane> {
        let (height, width) = term.size();
        // Sized to the full-sidebar pane: the 没入 `drive` loop resizes the pane to
        // the live sidebar state on attach, so a session collapsed to the rail
        // still fits the moment it is driven.
        let geo = ui::attached_geometry(height as usize, width as usize, Sidebar::Full);
        let (initial, cli) = match kind {
            // An agent pane sends its launch command and remembers its CLI (so the
            // open-panes snapshot can restore the same agent and resume it).
            PaneKind::Agent => (launch.agent_command, Some(launch.cli)),
            // A terminal pane opens a plain shell and has no agent to record.
            PaneKind::Terminal => (None, None),
        };
        if matches!(kind, PaneKind::Agent) {
            agent_state_store::clear(dir);
            self.lock().monitor.forget(dir);
        }
        let pty = PtySession::spawn(
            dir,
            geo.rows,
            geo.cols,
            initial,
            self.scrollback_lines,
            launch.env,
        )?;
        let id = self.allocate_pane_id();
        Ok(Pane {
            id,
            pty,
            kind,
            label_override: None,
            cli,
        })
    }

    fn allocate_pane_id(&mut self) -> u64 {
        let id = self.next_pane_id;
        self.next_pane_id = self.next_pane_id.wrapping_add(1);
        id
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
            // The shell pid of every pane — the roots the resource sampler totals
            // each session's process tree from. A pane already reaped reports
            // none and is simply left out.
            let roots = sp.panes.iter().filter_map(|p| p.pty.process_id()).collect();
            let pr_panes = sp
                .panes
                .iter()
                .map(|p| WatchedPrPane {
                    id: p.id,
                    parser: p.pty.parser_handle(),
                    generation: p.pty.generation_handle(),
                    // Force the watcher to scan once after registration, so a
                    // restored pane whose screen already contains a PR URL is
                    // folded into the sidebar without requiring more output.
                    last_generation: u64::MAX,
                    last_prs: Vec::new(),
                })
                .collect();
            let agent_input = sp
                .panes
                .iter()
                .find(|p| matches!(p.kind, PaneKind::Agent))
                .map(|p| p.pty.input_handle());
            Some(Watched {
                bell,
                alive,
                roots,
                pr_panes,
                agent_input,
                label: label.to_string(),
            })
        });
        let mut shared = self.lock();
        match watched {
            Some(watched) => {
                shared.sessions.insert(key, watched);
                self.version.fetch_add(1, Ordering::SeqCst);
                // Wake the watcher out of its no-session cheap path. Store before
                // dropping the lock so the next tick will take the lock and observe
                // the newly registered session.
                self.has_sessions.store(true, Ordering::Release);
            }
            None => {
                shared.sessions.remove(&key);
                shared.pr_link_updates.remove(&key);
                shared.monitor.forget(dir);
                agent_state_store::clear(dir);
                self.version.fetch_add(1, Ordering::SeqCst);
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
    /// Returns the worktree paths whose panes were removed, so the caller can also
    /// clear their persisted open-pane snapshots (a session recreated at the same
    /// path then starts fresh rather than restoring this run's panes).
    pub fn remove_under(&mut self, root: &Path) -> Vec<PathBuf> {
        let removed: Vec<PathBuf> = self
            .sessions
            .keys()
            .filter(|path| path.as_path() == root || path.starts_with(root))
            .cloned()
            .collect();
        if removed.is_empty() {
            return removed;
        }
        for path in &removed {
            // Dropping the PtySession kills the shell it owns.
            self.sessions.remove(path);
        }
        let mut shared = self.lock();
        for path in &removed {
            shared.sessions.remove(path);
            shared.pr_link_updates.remove(path);
            shared.monitor.forget(path);
            agent_state_store::clear(path);
        }
        drop(shared);
        removed
    }

    /// The open-pane snapshot for a single session (`dir`), or `None` when no
    /// session with panes is rooted there. The home screen persists this after a
    /// pane is attached / closed so the on-disk snapshot tracks the live panes.
    pub fn snapshot_open_panes_for(&self, dir: &Path) -> Option<(usize, Vec<StoredPane>)> {
        let sp = self.sessions.get(dir).filter(|sp| !sp.panes.is_empty())?;
        let panes = sp
            .panes
            .iter()
            .map(|p| StoredPane {
                kind: match p.kind {
                    PaneKind::Agent => StoredPaneKind::Agent,
                    PaneKind::Terminal => StoredPaneKind::Terminal,
                },
                cli: p.cli,
                label: p.label_override.clone(),
            })
            .collect();
        let active = sp.active.min(sp.panes.len().saturating_sub(1));
        Some((active, panes))
    }

    /// Snapshot the live terminal for the session rooted at `dir`, resized to the
    /// current pane geometry, for the sidebar's read-only preview. Returns `None`
    /// when no live session is rooted there, so the right pane falls back to the
    /// command log. Resizing here keeps a backgrounded session's screen reflowed
    /// to the visible pane, exactly as attaching to it would.
    pub fn snapshot(&mut self, term: &Term, dir: &Path, sidebar: Sidebar) -> Option<TerminalView> {
        let sp = self.sessions.get_mut(dir)?;
        if sp.panes.is_empty() {
            return None;
        }
        let active = sp.active.min(sp.panes.len().saturating_sub(1));
        let session = &mut sp.panes[active].pty;
        if !session.is_alive() {
            return None;
        }
        let (height, width) = term.size();
        // The preview draws the tab strip above the body (the same header + tab
        // rows 没入 shows), so it must size the snapshot to the tab-reserved
        // geometry — otherwise the grid is `TAB_BAR_ROWS` taller than the area it
        // is drawn into and the bottom rows (the live cursor) clip off, only to
        // reappear once the session is selected and reflowed to this same size.
        // 切替 honours the `Ctrl-B` sidebar toggle, so the snapshot is sized to the
        // current sidebar state — collapsing the rail widens the preview to match.
        let geo = ui::attached_geometry(height as usize, width as usize, sidebar);
        let generation = session.generation();
        // The previewed session, the pane geometry, and the shell's output are all
        // unchanged since the last frame: reuse the snapshot without touching the
        // parser lock at all. The 没入 `drive` loop differs the same way; this
        // brings the read-only preview in line with it.
        if let Some(cache) = &self.preview_cache {
            if cache.dir == dir
                && cache.active == active
                && cache.geo == geo
                && cache.generation == generation
            {
                return Some(cache.view.clone());
            }
        }
        // Reflow the backgrounded session to the visible pane only when its grid is
        // not already at this geometry. Gating on the session's actual size — not
        // merely "the previewed dir changed" — matters: the cache only remembers the
        // last previewed session, so moving the cursor back onto a session already
        // sized to the preview would still fire a redundant `resize`. That spurious
        // TIOCSWINSZ delivers a SIGWINCH to the program inside, and a full-screen TUI
        // (an agent's UI) answers by clearing and redrawing its whole screen; the
        // snapshot read just below catches it mid-redraw, so the preview flickers for
        // a frame or two on every such switch. Skipping the no-op resize keeps a
        // re-selected session steady, while a genuine size change (a `Ctrl-B` sidebar
        // toggle, or a session first sized differently) still reflows exactly once.
        if session.parser().screen().size() != (geo.rows, geo.cols) {
            session.resize(geo.rows, geo.cols);
        }
        let view = TerminalView::from_screen(session.parser().screen());
        self.preview_cache = Some(PreviewCache {
            dir: dir.to_path_buf(),
            active,
            geo,
            generation,
            view: view.clone(),
        });
        Some(view)
    }

    /// Recovers the guard rather than panicking if the lock was poisoned: the
    /// render loop reaches this through `snapshot` / `spawn_pane` / `refresh_watched`,
    /// and any thread that panicked while holding `Shared` would poison the mutex,
    /// so an `expect` here would escalate it into a crash of the whole TUI —
    /// leaving the terminal in raw mode. A possibly-stale view beats taking the UI
    /// down. Mirrors the watcher thread's poison handling and [`PtySession::parser`].
    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for TerminalPool {
    fn drop(&mut self) {
        // Stop the watcher and wait for it before touching the shells (it walks
        // no PTY state, but joining it first keeps the teardown ordering plain).
        self.stop.store(true, Ordering::SeqCst);
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
        // Tear the live shells down concurrently instead of letting the `sessions`
        // map drop them one by one. Each [`PtySession`]'s `Drop` bounds itself to
        // ~2s (kill → reap → reader-join sharing one deadline) for the pathological
        // case where a descendant escaped the process group; dropping panes in
        // sequence would stack that bound per pane, so quitting a workspace with
        // several open panes could freeze the UI for many seconds. Handing each
        // pane to its own thread caps the whole teardown at a single pane's bound
        // no matter how many are open — and in the common case every group dies in
        // parallel, so the reaps all return at once. `PtySession` is `Send` and
        // owns everything it touches, so each moved pane is self-contained.
        let handles: Vec<_> = std::mem::take(&mut self.sessions)
            .into_values()
            .flat_map(|session| session.panes)
            .map(|pane| std::thread::spawn(move || drop(pane)))
            .collect();
        for handle in handles {
            let _ = handle.join();
        }
    }
}

/// Spawn the watcher thread: every [`POLL_INTERVAL`] it prunes exited sessions,
/// feeds the live bell counts and recorded phases to the [`SessionMonitor`], and
/// fires a one-shot notification for each session that has just begun waiting for
/// input (background or attached) or whose background agent has just finished.
fn spawn_watcher(
    shared: Arc<Mutex<Shared>>,
    version: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    has_sessions: Arc<AtomicBool>,
    notifications_enabled: bool,
    mut sampler: Box<dyn ResourceSampler>,
) -> JoinHandle<()> {
    // One reader for the watcher's lifetime so its mtime cache survives across
    // ticks: an unchanged phase file then costs a single `stat`, not a re-read.
    let phase_reader = agent_state_store::PhaseReader::new();
    // Counts bell ticks so the heavier resource sample runs only every
    // `RESOURCE_SAMPLE_EVERY`th of them (≈ two seconds).
    let mut tick: u32 = 0;
    std::thread::spawn(move || loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(POLL_INTERVAL);
        // Nothing has ever been registered (or the previous locked tick observed
        // the map empty): keep the idle watcher down to one atomic load per tick,
        // instead of contending with render-time `snapshot()` on `Shared`.
        if !has_sessions.load(Ordering::Acquire) {
            continue;
        }

        // Snapshot the bookkeeping under the lock: prune dead sessions, observe
        // the phases/bells, and clone the lightweight handles needed by the
        // off-lock work below (PR scans and live-prompt delivery).
        let (notices, pr_jobs, live_prompt_targets): (
            Vec<(String, session_monitor::NoticeKind)>,
            Vec<PrScanJob>,
            Vec<LivePromptTarget>,
        ) = {
            let mut shared = match shared.lock() {
                Ok(shared) => shared,
                // The shared state's mutex is poisoned: a thread panicked while
                // holding it, so the bookkeeping can no longer be trusted and the
                // watcher must stop. Record why before breaking — otherwise every
                // session's bell / phase badge silently freezes with no trace.
                // (Best-effort, like every other failure in this thread; the
                // decision here is trivial enough — poison ⇒ fatal — to inline
                // rather than route through a tested layer.)
                Err(_) => {
                    crate::infrastructure::error_log::ErrorLog::record(
                        "terminal pool watcher stopped: shared state mutex poisoned",
                    );
                    break;
                }
            };
            let before = snapshot_locked(&shared);

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
            // Release phase-cache entries for sessions no longer tracked — those
            // pruned just above and those a session removal took straight out of
            // `shared.sessions` (which never enter the `dead` list). Keyed on the
            // live set so the cache cannot grow unbounded across a long run.
            phase_reader.retain(|path| shared.sessions.contains_key(path));
            let (notices, pr_jobs, live_prompt_targets) = if shared.sessions.is_empty() {
                shared.resources.clear();
                shared.resource_total = ResourceUsage::default();
                // The authoritative empty observation happens while holding the
                // lock. Future ticks can skip the lock until `refresh_watched`
                // registers a session and flips this back to true.
                has_sessions.store(false, Ordering::Release);
                (Vec::new(), Vec::new(), Vec::new())
            } else {
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
                let notices = shared
                    .monitor
                    .observe(&readings)
                    .into_iter()
                    .filter_map(|notice| {
                        shared
                            .sessions
                            .get(&notice.path)
                            .map(|w| (w.label.clone(), notice.kind))
                    })
                    .collect();
                let pr_jobs = pending_pr_scans(&mut shared);
                let live_prompt_targets = shared
                    .sessions
                    .iter()
                    .filter_map(|(path, watched)| {
                        watched.agent_input.as_ref().map(|input| LivePromptTarget {
                            path: path.clone(),
                            input: input.clone(),
                        })
                    })
                    .collect();
                (notices, pr_jobs, live_prompt_targets)
            };
            if snapshot_locked(&shared) != before {
                version.fetch_add(1, Ordering::SeqCst);
            }
            (notices, pr_jobs, live_prompt_targets)
        };

        let pr_results = scan_pr_jobs(pr_jobs);
        let merged_prs = persist_pr_results(&pr_results);
        if !pr_results.is_empty() {
            let pr_changed = {
                let mut shared = match shared.lock() {
                    Ok(shared) => shared,
                    Err(_) => {
                        crate::infrastructure::error_log::ErrorLog::record(
                            "terminal pool watcher stopped: shared state mutex poisoned",
                        );
                        break;
                    }
                };
                apply_pr_results(&mut shared, pr_results, merged_prs)
            };
            if pr_changed {
                version.fetch_add(1, Ordering::SeqCst);
            }
        }

        deliver_live_prompts(live_prompt_targets);

        if notifications_enabled {
            for (label, kind) in notices {
                notify(&label, kind);
            }
        }

        // Sample CPU / memory on the slower beat. The shell pids are read under
        // the lock, then the (heavy) system sample and the pure aggregation run
        // off-lock, and only the results are written back — so the render loops
        // contend for the mutex no longer than a bell poll already does. With no
        // live session the sample is skipped and the figures cleared, so an idle
        // workspace carries none.
        tick = tick.wrapping_add(1);
        if tick.is_multiple_of(RESOURCE_SAMPLE_EVERY) {
            let roots: Vec<(PathBuf, Vec<u32>)> = match shared.lock() {
                Ok(shared) => shared
                    .sessions
                    .iter()
                    .filter(|(_, w)| w.any_alive())
                    .map(|(path, w)| (path.clone(), w.roots.clone()))
                    .collect(),
                Err(_) => break,
            };
            let (resources, total) = if roots.is_empty() {
                (HashMap::new(), ResourceUsage::default())
            } else {
                let samples = sampler.sample();
                let (per_root, total) = aggregate_by_root(&samples, &roots);
                (per_root.into_iter().collect(), total)
            };
            if let Ok(mut shared) = shared.lock() {
                let changed = shared.resources != resources || shared.resource_total != total;
                shared.resources = resources;
                shared.resource_total = total;
                if changed {
                    version.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
    })
}

/// Drain prompts queued by the MCP live `session_prompt` tool and type them into
/// each session's live agent pane. Only sessions that had a live agent pane when the
/// watcher snapshotted them are passed here; sessions without one leave any
/// queued prompts on disk for a later pane to drain. A failed PTY write leaves
/// the remaining (undelivered) prompts requeued for a later tick and stops the
/// batch, so a wedged write never silently drops a prompt the sender was told
/// was queued; the failure is also logged for diagnosis.
fn deliver_live_prompts(targets: Vec<LivePromptTarget>) {
    for LivePromptTarget { path, input } in targets {
        let prompts = agent_live_prompt_store::take_all(&path);
        for (index, prompt) in prompts.iter().enumerate() {
            let bytes = pane_input::encode_prompt_submit(prompt, input.bracketed_paste());
            if let Err(err) = input.write(&bytes) {
                // The write failed, so this prompt and every one after it in the
                // batch are undelivered. Put them back at the front of the queue
                // (ahead of anything appended since the drain) so a later tick
                // retries them instead of losing prompts the sender was told were
                // queued. Then stop this session's batch — a broken pipe will only
                // fail the rest too.
                let undelivered = &prompts[index..];
                if let Err(requeue_err) = agent_live_prompt_store::requeue(&path, undelivered) {
                    error_log::ErrorLog::record(&format!(
                        "failed to requeue {} undelivered live prompt(s) for {}: {requeue_err:#}",
                        undelivered.len(),
                        path.display()
                    ));
                }
                error_log::ErrorLog::record(&format!(
                    "failed to inject live prompt into {}: {err:#}",
                    path.display()
                ));
                break;
            }
        }
    }
}

/// Show a desktop notification that a session changed state: it began waiting for
/// input (background or attached), or a background agent finished.
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
        Self::new(
            true,
            crate::domain::settings::DEFAULT_TERMINAL_SCROLLBACK_LINES,
        )
    }
}

//! The single in-flight background install, shared across every TUI screen.
//!
//! Some background work takes a while — provisioning the local LLM (`ollama`
//! runtime + model) can take minutes, and a self-update downloads a fresh release
//! — so it runs on a background thread and the user is free to keep using usagi
//! while it proceeds. This module owns the one process-global [`InstallHandle`]
//! that the worker writes its progress to and that every screen reads: the
//! [`FramePainter`](super::io::screen::FramePainter) overlays a loading rabbit from
//! it, and the event loops poll [`is_active`](InstallHandle::is_active) to keep
//! the screen animating while the work runs. The caller supplies the label shown
//! beside the rabbit (e.g. `LLM 導入中… <model>` or `アップデート中…`), so the
//! handle stays agnostic about what kind of work is in flight.
//!
//! The thread spawn itself lives in the (test-excluded) config screen module —
//! mirroring how the home screen spawns the update check while
//! [`update`](super::home::update) holds the shared handle — so the logic here
//! (state transitions, the time-derived animation) stays pure and testable
//! without ever shelling out.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::presentation::tui::widgets;

/// How often the hop pose and braille spinner advance. Fast, so the rabbit reads
/// as lively motion. Also the cadence at which a waiting screen repaints the
/// overlay (see [`animated_read`](super::io::screen::animated_read)).
pub const ANIM_TICK: Duration = Duration::from_millis(110);

/// How often the facial expression changes. Slow and **independent of progress**
/// — the face simply shifts on the clock so the rabbit's mood drifts while it
/// works, never implying a percentage that does not exist.
const FACE_TICK: Duration = Duration::from_millis(1500);

/// How long the completion message lingers after the install finishes before the
/// overlay clears itself.
const DISMISS: Duration = Duration::from_secs(6);

/// The lifecycle of the one tracked install.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum TaskState {
    /// No install has run (or the last one's message has been dismissed).
    #[default]
    Idle,
    /// An install is in progress, started at `started`. `label` is the caller's
    /// description shown beside the loading rabbit.
    Running { label: String, started: Instant },
    /// The install finished at `finished`; `message` is shown until [`DISMISS`].
    Done {
        message: String,
        ok: bool,
        finished: Instant,
    },
}

/// What a screen should draw for the install right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallView {
    /// In progress: a hopping rabbit whose hop/face indices are derived from
    /// elapsed time (see [`ANIM_TICK`] / [`FACE_TICK`]).
    Running {
        label: String,
        hop_frame: usize,
        face_index: usize,
    },
    /// Just finished: a short message with a happy (or sad) face.
    Done { message: String, ok: bool },
}

/// The hop/spinner frame for `elapsed`.
fn hop_frame(elapsed: Duration) -> usize {
    (elapsed.as_millis() / ANIM_TICK.as_millis()) as usize
}

/// The facial-expression index for `elapsed`.
fn face_index(elapsed: Duration) -> usize {
    (elapsed.as_millis() / FACE_TICK.as_millis()) as usize
}

/// A cloneable handle onto the one tracked install.
///
/// Cloning shares the same slot, so the worker thread's
/// [`finish`](Self::finish) is visible to every reader. A fresh handle (the
/// default) is [`Idle`](TaskState::Idle): nothing is drawn until an install
/// begins.
#[derive(Clone, Default)]
pub struct InstallHandle {
    shared: Arc<Mutex<TaskState>>,
}

impl InstallHandle {
    /// A handle with no install in flight.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TaskState> {
        // Recover a poisoned lock rather than propagating the panic. This handle
        // is read by every screen's animated overlay while the terminal is in raw
        // / alternate-screen mode, so escalating a poison here would crash the UI
        // with the terminal left broken. A stale `TaskState` reading is the worst
        // outcome. Matches the never-crash-on-poison policy of the sibling handles
        // (`UpdateHandle`, `SessionsRefreshHandle`, the terminal pool / monitor).
        self.shared.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Mark an install labelled `label` as started at `at`. Returns `false` (a
    /// no-op) when one is already running, so a second trigger cannot start a
    /// duplicate install over the first.
    pub fn begin_at(&self, label: &str, at: Instant) -> bool {
        let mut state = self.lock();
        if matches!(&*state, TaskState::Running { .. }) {
            return false;
        }
        *state = TaskState::Running {
            label: label.to_string(),
            started: at,
        };
        true
    }

    /// Mark an install as started now (production entry point).
    pub fn begin(&self, label: &str) -> bool {
        self.begin_at(label, Instant::now())
    }

    /// Record that the install finished at `at`, with the message to show.
    pub fn finish_at(&self, ok: bool, message: String, at: Instant) {
        *self.lock() = TaskState::Done {
            message,
            ok,
            finished: at,
        };
    }

    /// Record that the install finished now (production entry point, called from
    /// the worker thread).
    pub fn finish(&self, ok: bool, message: String) {
        self.finish_at(ok, message, Instant::now());
    }

    /// Whether something should still be drawn for the install at `now`: an
    /// in-progress run, or a finished one whose message has not yet timed out.
    pub fn is_active(&self, now: Instant) -> bool {
        match &*self.lock() {
            TaskState::Idle => false,
            TaskState::Running { .. } => true,
            TaskState::Done { finished, .. } => now.saturating_duration_since(*finished) < DISMISS,
        }
    }

    /// What to draw for the install at `now`, or `None` when nothing is in
    /// flight. A `Done` message that has outlived [`DISMISS`] clears the slot
    /// back to [`Idle`](TaskState::Idle) as a side effect, so the overlay
    /// disappears on its own.
    pub fn view(&self, now: Instant) -> Option<InstallView> {
        let mut state = self.lock();
        match &*state {
            TaskState::Idle => None,
            TaskState::Running { label, started } => {
                let elapsed = now.saturating_duration_since(*started);
                Some(InstallView::Running {
                    label: label.clone(),
                    hop_frame: hop_frame(elapsed),
                    face_index: face_index(elapsed),
                })
            }
            TaskState::Done {
                message,
                ok,
                finished,
            } => {
                if now.saturating_duration_since(*finished) >= DISMISS {
                    *state = TaskState::Idle;
                    None
                } else {
                    Some(InstallView::Done {
                        message: message.clone(),
                        ok: *ok,
                    })
                }
            }
        }
    }
}

/// The process-global handle the worker writes to and every screen reads.
fn global() -> &'static InstallHandle {
    static HANDLE: OnceLock<InstallHandle> = OnceLock::new();
    HANDLE.get_or_init(InstallHandle::new)
}

/// A clone of the global install handle, for the screens that drive the
/// animated read against it.
pub fn handle() -> InstallHandle {
    global().clone()
}

/// What every screen should draw for the global install right now (`None` when
/// nothing is in flight).
pub fn snapshot() -> Option<InstallView> {
    global().view(Instant::now())
}

/// Render an [`InstallView`] into the top-right corner of `frame` (in place),
/// the way every screen surfaces the background install. A no-op when `view` is
/// `None`.
pub fn overlay(frame: &mut [String], width: usize, view: Option<&InstallView>) {
    let banner = match view {
        Some(InstallView::Running {
            label,
            hop_frame,
            face_index,
        }) => widgets::loading_rabbit_timed(*hop_frame, *face_index, label),
        Some(InstallView::Done { message, ok }) => widgets::done_rabbit(*ok, message),
        None => return,
    };
    widgets::overlay_top_right(frame, 0, width, &banner);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_handle_is_idle() {
        let handle = InstallHandle::new();
        let now = Instant::now();
        assert!(!handle.is_active(now));
        assert_eq!(handle.view(now), None);
    }

    #[test]
    fn begin_marks_it_running_and_blocks_a_duplicate() {
        let handle = InstallHandle::new();
        let t0 = Instant::now();
        assert!(handle.begin_at("LLM 導入中… qwen2.5:7b", t0));
        // A second begin while running is refused, leaving the first untouched.
        assert!(!handle.begin_at("other", t0));
        assert!(handle.is_active(t0));
        // At t0 nothing has elapsed, so the hop and face sit at frame 0. The label
        // is the caller's string verbatim.
        assert_eq!(
            handle.view(t0),
            Some(InstallView::Running {
                label: "LLM 導入中… qwen2.5:7b".to_string(),
                hop_frame: 0,
                face_index: 0,
            })
        );
    }

    #[test]
    fn running_view_derives_hop_and_face_from_elapsed_time() {
        let handle = InstallHandle::new();
        let t0 = Instant::now();
        handle.begin_at("m", t0);
        // After 330ms the hop has advanced three ANIM_TICKs; the face is still on
        // its first FACE_TICK bucket (face changes only every 1500ms).
        let view = handle.view(t0 + Duration::from_millis(330)).unwrap();
        assert_eq!(
            view,
            InstallView::Running {
                label: "m".to_string(),
                hop_frame: 3,
                face_index: 0,
            }
        );
        // Past one FACE_TICK the expression has moved on, independent of any
        // progress signal: at 1600ms the hop is at 1600/110 = 14 and the face at
        // 1600/1500 = 1.
        assert_eq!(
            handle.view(t0 + Duration::from_millis(1600)),
            Some(InstallView::Running {
                label: "m".to_string(),
                hop_frame: 14,
                face_index: 1,
            })
        );
    }

    #[test]
    fn finish_shows_the_message_until_it_times_out_then_clears() {
        let handle = InstallHandle::new();
        let t0 = Instant::now();
        handle.begin_at("m", t0);
        handle.finish_at(true, "done 🐰".to_string(), t0);
        // Within the dismiss window the message shows and the overlay is active.
        assert!(handle.is_active(t0 + Duration::from_secs(1)));
        assert_eq!(
            handle.view(t0 + Duration::from_secs(1)),
            Some(InstallView::Done {
                message: "done 🐰".to_string(),
                ok: true,
            })
        );
        // Past the window it is no longer active, and reading the view clears the
        // slot back to idle.
        assert!(!handle.is_active(t0 + DISMISS));
        assert_eq!(handle.view(t0 + DISMISS), None);
        assert_eq!(handle.view(t0 + DISMISS + Duration::from_secs(1)), None);
    }

    #[test]
    fn begin_after_finish_starts_a_fresh_run() {
        // A finished install is not "Running", so a new one may begin.
        let handle = InstallHandle::new();
        let t0 = Instant::now();
        handle.begin_at("a", t0);
        handle.finish_at(false, "failed".to_string(), t0);
        assert!(handle.begin_at("b", t0 + Duration::from_secs(1)));
    }

    #[test]
    fn begin_and_finish_now_wrappers_drive_the_clock() {
        // The production wrappers stamp the current instant; exercise them so the
        // Instant::now() seams are covered.
        let handle = InstallHandle::new();
        assert!(handle.begin("m"));
        assert!(handle.is_active(Instant::now()));
        handle.finish(true, "ok".to_string());
        assert!(handle.is_active(Instant::now()));
    }

    #[test]
    fn lock_recovers_from_a_poisoned_mutex_instead_of_crashing() {
        // A thread that panics while holding the lock poisons the mutex. The
        // handle must still hand back the last state (recovering the inner value)
        // rather than propagating the poison and crashing the screen that draws
        // the overlay from it.
        let handle = InstallHandle::new();
        let t0 = Instant::now();
        handle.begin_at("m", t0);
        let clone = handle.clone();
        let _ = std::thread::spawn(move || {
            let _guard = clone.shared.lock().unwrap();
            panic!("poison the mutex");
        })
        .join();
        // The slot is still readable and reports the install still running.
        assert!(handle.is_active(t0));
    }

    #[test]
    fn the_global_handle_starts_idle() {
        // The process-global accessor returns an idle handle until production
        // begins an install (no test ever mutates the global, so this is stable).
        assert_eq!(snapshot(), None);
        assert!(!handle().is_active(Instant::now()));
    }

    #[test]
    fn overlay_draws_running_and_done_and_skips_none() {
        let blank = || vec![String::new(); 4];

        let mut running = blank();
        overlay(
            &mut running,
            80,
            Some(&InstallView::Running {
                label: "LLM 導入中… m".to_string(),
                hop_frame: 0,
                face_index: 0,
            }),
        );
        let joined = console::strip_ansi_codes(&running.join("\n")).into_owned();
        assert!(joined.contains("LLM 導入中… m"));

        let mut done = blank();
        overlay(
            &mut done,
            80,
            Some(&InstallView::Done {
                message: "完了".to_string(),
                ok: true,
            }),
        );
        let joined = console::strip_ansi_codes(&done.join("\n")).into_owned();
        assert!(joined.contains("完了"));

        let mut none = blank();
        overlay(&mut none, 80, None);
        assert!(none.iter().all(|l| l.is_empty()));
    }
}

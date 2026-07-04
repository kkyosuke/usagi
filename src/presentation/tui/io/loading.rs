//! Run a slow, blocking step on a worker thread while the screen shows an
//! animated loading rabbit.
//!
//! Some actions the TUI takes are genuine blocking external IO that can stall
//! for seconds — resolving 1Password-backed env before a pane launches (one `op`
//! subprocess per binding) and probing installed agent CLIs / the local LLM
//! before the config screen opens (a `--version` / `ollama` subprocess each).
//! Run inline on the UI thread they freeze the screen with no feedback. This
//! helper moves the work to a worker thread and, from the UI thread, repaints a
//! centred [`loading_screen`](crate::presentation::tui::widgets::loading_screen)
//! every [`ANIM_TICK`](crate::presentation::tui::install_task::ANIM_TICK) so the
//! rabbit hops on the clock until the work finishes.
//!
//! Everything here is real IO — a thread spawn, the animation clock, and the
//! terminal paint — so it holds no testable logic (the loading frame itself is
//! built by the covered [`loading_screen`] widget) and is excluded from coverage
//! (see `scripts/coverage.sh`).

use std::time::{Duration, Instant};

use console::Term;

use crate::presentation::tui::install_task::ANIM_TICK;
use crate::presentation::tui::io::screen::FramePainter;
use crate::presentation::tui::widgets;

/// How long the facial expression holds before advancing — matched to the
/// background-install overlay's own face cadence so the loading rabbit here
/// changes mood at the same pace.
const FACE_TICK: Duration = Duration::from_millis(1500);

/// Work that finishes within this grace period never paints the loading screen,
/// so a fast probe/resolve does not flash a splash the user barely sees. Only a
/// genuinely slow step (the case worth reassuring the user about) shows it.
const GRACE: Duration = Duration::from_millis(150);

/// How often the grace period polls the worker before deciding the work is slow
/// enough to warrant the loading screen.
const GRACE_POLL: Duration = Duration::from_millis(20);

/// Run `work` on a worker thread, animating a full-screen centred loading rabbit
/// labelled `label` until it finishes, then return its result.
///
/// Returns `None` if the worker thread panics (the caller falls back to a sane
/// default rather than crashing the TUI). Work that completes within [`GRACE`]
/// returns without ever painting, so only a slow step shows the loading screen.
///
/// The full-screen splash suits a step that owns the whole screen (opening the
/// config screen). A step that happens *within* a live surface — resolving a
/// pane's 1Password env is part of launching that pane, so it belongs in the tab
/// context around it — drives [`run_with_loading_frames`] instead and floats its
/// indicator over the surface rather than blanking it.
pub fn run_with_loading<T, W>(term: &Term, label: &str, work: W) -> Option<T>
where
    T: Send + 'static,
    W: FnOnce() -> T + Send + 'static,
{
    run_with_loading_frames(term, work, |hop, face, height, width| {
        widgets::loading_screen(width, height, hop, face, label)
    })
}

/// Like [`run_with_loading`] but the caller supplies the whole frame to paint on
/// each animation tick via `frame_for(hop, face, height, width)`, so the loading
/// indicator can be composited over a live surface (e.g. floated in the right
/// pane over the home screen) instead of a full-screen splash. `hop` / `face` are
/// the time-derived hop and expression indices; `height` / `width` are the
/// terminal size that tick. Runs on the UI thread — it may borrow screen state —
/// while `work` runs on the worker thread.
///
/// Grace and panic handling match [`run_with_loading`]: work finishing within
/// [`GRACE`] never paints, and a panicked worker returns `None`.
pub fn run_with_loading_frames<T, W, F>(term: &Term, work: W, frame_for: F) -> Option<T>
where
    T: Send + 'static,
    W: FnOnce() -> T + Send + 'static,
    F: Fn(usize, usize, usize, usize) -> Vec<String>,
{
    let started = Instant::now();
    let worker = std::thread::spawn(work);

    // Grace period: wait briefly without painting so fast work shows nothing.
    while !worker.is_finished() {
        if started.elapsed() >= GRACE {
            break;
        }
        std::thread::sleep(GRACE_POLL);
    }

    // Slow path: animate the caller's frame until the worker is done. A fresh
    // painter starts from a blank remembered frame, so its first paint clears the
    // screen and the whole frame is drawn.
    let mut painter = FramePainter::new();
    while !worker.is_finished() {
        let elapsed = started.elapsed();
        let (height, width) = term.size();
        let frame = frame_for(
            hop_frame(elapsed),
            face_index(elapsed),
            height as usize,
            width as usize,
        );
        let _ = painter.paint(term, frame);
        std::thread::sleep(ANIM_TICK);
    }

    worker.join().ok()
}

/// The hop / spinner frame for `elapsed`, advancing one step per [`ANIM_TICK`].
fn hop_frame(elapsed: Duration) -> usize {
    (elapsed.as_millis() / ANIM_TICK.as_millis()) as usize
}

/// The facial-expression index for `elapsed`, advancing one step per [`FACE_TICK`].
fn face_index(elapsed: Duration) -> usize {
    (elapsed.as_millis() / FACE_TICK.as_millis()) as usize
}

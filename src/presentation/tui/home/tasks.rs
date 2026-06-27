//! The background session tasks the home screen runs off the event-loop thread.
//!
//! Creating or removing a session shells out to git (worktree add / submodule
//! init / worktree remove) and can take seconds, so usagi runs each on its own
//! thread and the user keeps operating the screen meanwhile, instead of the loop
//! freezing until the git work returns. This module owns the one shared
//! [`TaskHandle`] the workers write to and the event loop reads: the loop drains
//! finished work to apply ([`drain_completed`](TaskHandle::drain_completed)) and
//! renders a stacked panel of what is still running ([`view`](TaskHandle::view)).
//!
//! It mirrors [`update`](super::update) / [`install_task`](super::super::install_task):
//! a cloneable `Arc<Mutex<_>>` handle, a **time-derived** spinner (a running
//! task's animation advances on the clock — git reports no per-task percentage,
//! so none is implied), and a short dismissal window after a task finishes so its
//! result lingers before the row clears itself. The renderer (`task_status_line`
//! in `super::ui`) does show a progress bar, but it scales on a **real** ratio —
//! how many of the tracked tasks have finished ([`TaskMark::Done`]) out of the
//! total — not a fabricated per-task percentage.
//!
//! The state transitions and the time-derived view are pure and tested here; the
//! thread spawn that drives a real task lives in the (coverage-excluded) home
//! screen module, exactly as the update check's spawn does.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::domain::workspace_state::SessionRecord;

#[cfg(test)]
use super::state::LineKind;
use super::state::LogLine;

/// How often a running task's braille spinner advances. Fast, so the row reads
/// as live motion while the git work proceeds.
const SPIN_TICK: Duration = Duration::from_millis(110);

/// The shortest time a task's spinner shows before its row may flip to a result.
/// A session create / remove often finishes in well under a second, so without
/// this floor the spinner would barely move before snapping to a static result —
/// reading as a frozen animation. Holding the spinner for at least one near-full
/// braille cycle makes even an instant task read as live work that then settles.
const MIN_SPIN: Duration = Duration::from_millis(700);

/// How long a finished task's row lingers in the panel after it completes before
/// [`view`](TaskHandle::view) drops it. Long enough to read the result, short
/// enough that the panel keeps pace with frequent session operations.
const DISMISS: Duration = Duration::from_secs(4);

/// The kind of background session task — what the worker is doing, used to label
/// its row in the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    /// Creating a session (git worktree add + submodule init).
    CreateSession,
    /// Removing a session (git worktree remove + branch delete + cleanup).
    RemoveSession,
}

impl TaskKind {
    /// The verb shown in the row label (`作成` / `削除`).
    fn verb(self) -> &'static str {
        match self {
            TaskKind::CreateSession => "作成",
            TaskKind::RemoveSession => "削除",
        }
    }

    /// The English operation name written to the persisted error log, matching
    /// the wording `run_create` / `run_remove` use for their own failure lines
    /// (`session create` / `session remove`).
    fn op_label(self) -> &'static str {
        match self {
            TaskKind::CreateSession => "session create",
            TaskKind::RemoveSession => "session remove",
        }
    }
}

/// What a worker hands back when its task finishes, for the event loop to apply
/// to the screen on the next frame: the line to log, the refreshed session list
/// (when the action changed it), and, for a removal, the pool path whose live
/// shell to evict (done on the UI thread, since the pool is not `Send`).
#[derive(Debug, Clone)]
pub struct Completion {
    /// A line describing the result (success or failure) to append to the log.
    pub line: LogLine,
    /// The refreshed session list, when the action changed it; `None` on a
    /// failure that left the sessions untouched.
    pub sessions: Option<Vec<SessionRecord>>,
    /// The session root whose embedded shell to evict from the pool — set on a
    /// successful removal so a session later recreated at the same path starts
    /// fresh. `None` for creations and failures.
    pub evict: Option<PathBuf>,
    /// The branch of a freshly created session to drop into 在席 (Focus) once the
    /// refreshed list lands — set only on a successful TUI-initiated create, so
    /// the user starts operating the new session without navigating to it. `None`
    /// for removals and failures (and, by construction, for MCP-driven creates,
    /// which never build a [`Completion`]).
    pub focus: Option<String>,
}

/// Decode the message a worker thread panicked with, from the payload
/// [`catch_unwind`](std::panic::catch_unwind) returns. Rust panics carry either a
/// `&str` (`panic!("…")`) or a `String` (`panic!("{x}")`); anything else is
/// reported as an opaque payload so the log entry still says *something*.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

/// What to do with a session worker that panicked: the line to persist to the
/// error log, and the failed [`Completion`] for its task row.
///
/// A panicked worker never reaches [`complete`](TaskHandle::complete), so without
/// this its row would spin forever and the panic — the very thing that poisons
/// the op-lock — would vanish with the dead thread. The home module catches the
/// unwind and routes the payload here; keeping the decoding and wording in this
/// (tested) module rather than the coverage-excluded home screen is why this is a
/// free function taking the payload.
pub fn panic_outcome(
    kind: TaskKind,
    target: &str,
    payload: Box<dyn std::any::Any + Send>,
) -> (String, Completion) {
    let message = panic_message(payload.as_ref());
    // The raw panic payload (e.g. `called Result::unwrap() on an Err value: …`)
    // is developer diagnostics, so it goes to the file log — but the on-screen
    // line shows a plain user-facing message instead, pointing at the log rather
    // than spilling internal text into the UI.
    let log_line = format!(
        "{} \"{target}\" worker panicked: {message}",
        kind.op_label()
    );
    let completion = Completion {
        line: LogLine::error(format!(
            "{}が異常終了しました（{target}）。詳細はログを確認してください",
            kind.verb()
        )),
        sessions: None,
        evict: None,
        focus: None,
    };
    (log_line, completion)
}

/// The mark drawn at the head of a task row: a spinner while it runs, or a
/// success / failure glyph once it finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskMark {
    /// In progress: the `usize` is a monotonically advancing spinner frame
    /// (derived from elapsed time), which the renderer maps to a braille glyph.
    Running(usize),
    /// Finished: `true` succeeded, `false` failed.
    Done(bool),
}

/// One row of the task panel: the label to show and its leading mark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    pub label: String,
    pub mark: TaskMark,
}

/// Where a single task sits in its lifecycle.
enum Phase {
    /// Running, started at `started` (the spinner frame is derived from it).
    Running { started: Instant },
    /// Finished at `finished`; `ok` styles the mark. `started` is kept so the
    /// spinner can be held for [`MIN_SPIN`] before the row flips to the mark, and
    /// the row lingers until [`DISMISS`] after `finished`.
    Done {
        ok: bool,
        started: Instant,
        finished: Instant,
    },
}

/// One tracked task.
struct Task {
    kind: TaskKind,
    target: String,
    phase: Phase,
}

impl Task {
    /// The panel row for this task at `now` (the spinner frame for a running
    /// task is derived from how long it has run). A finished task keeps spinning
    /// until it has shown its spinner for [`MIN_SPIN`], so an instant task still
    /// reads as live work before its result settles in.
    fn row(&self, now: Instant) -> TaskRow {
        match self.phase {
            Phase::Running { started } => self.running_row(started, now),
            Phase::Done { started, .. } if now.saturating_duration_since(started) < MIN_SPIN => {
                self.running_row(started, now)
            }
            Phase::Done { ok, .. } => {
                let suffix = if ok { "完了" } else { "失敗" };
                TaskRow {
                    label: format!("{}{} {}", self.kind.verb(), suffix, self.target),
                    mark: TaskMark::Done(ok),
                }
            }
        }
    }

    /// The spinning row for a task that began at `started`, shown both while it
    /// runs and during the [`MIN_SPIN`] hold after a fast finish.
    fn running_row(&self, started: Instant, now: Instant) -> TaskRow {
        let frame =
            (now.saturating_duration_since(started).as_millis() / SPIN_TICK.as_millis()) as usize;
        TaskRow {
            label: format!("{}中… {}", self.kind.verb(), self.target),
            mark: TaskMark::Running(frame),
        }
    }

    /// Whether the task should still be drawn at `now`: always while running, and
    /// after it finishes only until its [`DISMISS`] window closes.
    fn visible(&self, now: Instant) -> bool {
        match self.phase {
            Phase::Running { .. } => true,
            Phase::Done { finished, .. } => now.saturating_duration_since(finished) < DISMISS,
        }
    }
}

/// The shared board the workers write to and the event loop reads.
#[derive(Default)]
struct Board {
    /// The tracked tasks, in the order they were begun (so the panel stacks them
    /// oldest-first).
    tasks: Vec<(u64, Task)>,
    /// The id of the next task begun, so [`complete`](TaskHandle::complete) can
    /// address the right row.
    next_id: u64,
    /// The mailbox of finished-task outcomes the event loop has not yet drained.
    completed: Vec<Completion>,
}

/// A cloneable handle onto the one shared task board.
///
/// Cloning shares the same board, so a worker thread's
/// [`complete`](Self::complete) is visible to the event loop that reads it. A
/// fresh handle (the default) tracks nothing.
#[derive(Clone, Default)]
pub struct TaskHandle {
    shared: Arc<Mutex<Board>>,
}

impl TaskHandle {
    /// A handle tracking no tasks.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Board> {
        // Recover a poisoned lock rather than propagating the panic. This board
        // is read by the TUI event loop while the terminal is in raw /
        // alternate-screen mode; escalating a poison here would crash the UI with
        // the terminal left broken. The same never-crash-on-poison policy the
        // terminal pool documents — the lock only guards `Vec` pushes/finds and
        // an integer bump, so a recovered guard is safe to keep using.
        self.shared.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Register a task for `target` as begun (running) at `at`, returning its id
    /// for the worker to [`complete`](Self::complete) once the work is done.
    pub fn begin_at(&self, kind: TaskKind, target: &str, at: Instant) -> u64 {
        let mut board = self.lock();
        let id = board.next_id;
        board.next_id += 1;
        board.tasks.push((
            id,
            Task {
                kind,
                target: target.to_string(),
                phase: Phase::Running { started: at },
            },
        ));
        id
    }

    /// Register a task as begun now (production entry point, called from the
    /// event loop before it spawns the worker).
    pub fn begin(&self, kind: TaskKind, target: &str) -> u64 {
        self.begin_at(kind, target, Instant::now())
    }

    /// Record the task `id` as finished at `at` with the given outcome: flip its
    /// row to a success / failure mark and queue `completion` for the event loop
    /// to apply. A no-op for an unknown id (the row was already pruned).
    pub fn complete_at(&self, id: u64, ok: bool, completion: Completion, at: Instant) {
        let mut board = self.lock();
        if let Some((_, task)) = board.tasks.iter_mut().find(|(i, _)| *i == id) {
            let started = match task.phase {
                Phase::Running { started } | Phase::Done { started, .. } => started,
            };
            task.phase = Phase::Done {
                ok,
                started,
                finished: at,
            };
        }
        board.completed.push(completion);
    }

    /// Record the task `id` as finished now (production entry point, called from
    /// the worker thread).
    pub fn complete(&self, id: u64, ok: bool, completion: Completion) {
        self.complete_at(id, ok, completion, Instant::now());
    }

    /// Take the finished-task outcomes queued since the last drain, leaving the
    /// mailbox empty. The event loop applies each one (logging the line and
    /// refreshing the session list) exactly once.
    pub fn drain_completed(&self) -> Vec<Completion> {
        std::mem::take(&mut self.lock().completed)
    }

    /// Whether anything should still be drawn (and the screen kept animating) at
    /// `now`: a running task, or a finished one still inside its dismissal window.
    pub fn is_active(&self, now: Instant) -> bool {
        self.lock().tasks.iter().any(|(_, task)| task.visible(now))
    }

    /// The rows to render at `now`, oldest task first. Finished rows past their
    /// dismissal window are pruned as a side effect, so the panel empties itself.
    pub fn view(&self, now: Instant) -> Vec<TaskRow> {
        let mut board = self.lock();
        board.tasks.retain(|(_, task)| task.visible(now));
        board.tasks.iter().map(|(_, task)| task.row(now)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completion() -> Completion {
        Completion {
            line: LogLine::output("done"),
            sessions: None,
            evict: None,
            focus: None,
        }
    }

    #[test]
    fn a_fresh_handle_tracks_nothing() {
        let handle = TaskHandle::new();
        let now = Instant::now();
        assert!(!handle.is_active(now));
        assert!(handle.view(now).is_empty());
        assert!(handle.drain_completed().is_empty());
    }

    #[test]
    fn lock_recovers_from_a_poisoned_mutex_instead_of_crashing() {
        // A thread that panics while holding the board lock poisons the mutex.
        // The handle must still hand back a usable guard rather than propagating
        // the poison and crashing the TUI event loop that reads the board.
        let handle = TaskHandle::new();
        let id = handle.begin(TaskKind::CreateSession, "sess");
        let clone = handle.clone();
        let _ = std::thread::spawn(move || {
            let _guard = clone.shared.lock().unwrap();
            panic!("poison the mutex");
        })
        .join();
        // The board is still usable: the previously-begun task is observable and
        // can be completed without a crash.
        let now = Instant::now();
        assert!(handle.is_active(now));
        handle.complete_at(id, true, completion(), now);
        assert!(!handle.drain_completed().is_empty());
    }

    #[test]
    fn begin_shows_a_running_row_with_a_time_derived_spinner() {
        let handle = TaskHandle::new();
        let t0 = Instant::now();
        let id = handle.begin_at(TaskKind::CreateSession, "main", t0);
        assert_eq!(id, 0);
        assert!(handle.is_active(t0));
        // At t0 nothing has elapsed, so the spinner sits at frame 0.
        assert_eq!(
            handle.view(t0),
            vec![TaskRow {
                label: "作成中… main".to_string(),
                mark: TaskMark::Running(0),
            }]
        );
        // After 330ms the spinner has advanced three SPIN_TICKs.
        assert_eq!(
            handle.view(t0 + Duration::from_millis(330)),
            vec![TaskRow {
                label: "作成中… main".to_string(),
                mark: TaskMark::Running(3),
            }]
        );
    }

    #[test]
    fn complete_marks_done_queues_the_completion_and_drains_once() {
        let handle = TaskHandle::new();
        let t0 = Instant::now();
        let id = handle.begin_at(TaskKind::RemoveSession, "feat/x", t0);
        handle.complete_at(id, true, completion(), t0);
        // Once the spinner has shown for MIN_SPIN the row flips to a success mark…
        assert_eq!(
            handle.view(t0 + MIN_SPIN),
            vec![TaskRow {
                label: "削除完了 feat/x".to_string(),
                mark: TaskMark::Done(true),
            }]
        );
        // …and the completion is handed out exactly once.
        let drained = handle.drain_completed();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].line.text, "done");
        assert!(handle.drain_completed().is_empty());
    }

    #[test]
    fn a_failed_task_reads_as_failure() {
        let handle = TaskHandle::new();
        let t0 = Instant::now();
        let id = handle.begin_at(TaskKind::CreateSession, "dup", t0);
        handle.complete_at(id, false, completion(), t0);
        assert_eq!(
            handle.view(t0 + MIN_SPIN),
            vec![TaskRow {
                label: "作成失敗 dup".to_string(),
                mark: TaskMark::Done(false),
            }]
        );
    }

    #[test]
    fn an_instant_finish_still_spins_for_min_spin_before_settling() {
        // A task that finishes almost immediately keeps showing its spinner until
        // MIN_SPIN has elapsed, so the row reads as live work rather than a result
        // that flashes in with no motion.
        let handle = TaskHandle::new();
        let t0 = Instant::now();
        let id = handle.begin_at(TaskKind::CreateSession, "fast", t0);
        handle.complete_at(id, true, completion(), t0);
        // Right after finishing, the row is still the spinner (frame advancing).
        assert_eq!(
            handle.view(t0 + Duration::from_millis(330)),
            vec![TaskRow {
                label: "作成中… fast".to_string(),
                mark: TaskMark::Running(3),
            }]
        );
        // It stays active through the hold so the screen keeps animating.
        assert!(handle.is_active(t0 + Duration::from_millis(330)));
        // Only once MIN_SPIN has passed does it settle to the result mark.
        assert_eq!(
            handle.view(t0 + MIN_SPIN),
            vec![TaskRow {
                label: "作成完了 fast".to_string(),
                mark: TaskMark::Done(true),
            }]
        );
    }

    #[test]
    fn a_finished_row_lingers_then_prunes_itself() {
        let handle = TaskHandle::new();
        let t0 = Instant::now();
        let id = handle.begin_at(TaskKind::RemoveSession, "old", t0);
        handle.complete_at(id, true, completion(), t0);
        // Within the dismiss window it still shows and keeps the screen active.
        assert!(handle.is_active(t0 + Duration::from_secs(1)));
        assert_eq!(handle.view(t0 + Duration::from_secs(1)).len(), 1);
        // Past the window it is no longer active, and reading the view prunes it.
        assert!(!handle.is_active(t0 + DISMISS));
        assert!(handle.view(t0 + DISMISS).is_empty());
    }

    #[test]
    fn completing_an_unknown_id_only_queues_the_completion() {
        // A stale id (its row was already pruned) leaves the rows untouched but
        // the completion is still delivered, so the outcome is never lost.
        let handle = TaskHandle::new();
        let t0 = Instant::now();
        handle.complete_at(999, true, completion(), t0);
        assert!(handle.view(t0).is_empty());
        assert_eq!(handle.drain_completed().len(), 1);
    }

    #[test]
    fn multiple_tasks_stack_oldest_first() {
        let handle = TaskHandle::new();
        let t0 = Instant::now();
        handle.begin_at(TaskKind::CreateSession, "a", t0);
        handle.begin_at(TaskKind::RemoveSession, "b", t0);
        let rows = handle.view(t0);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label, "作成中… a");
        assert_eq!(rows[1].label, "削除中… b");
    }

    #[test]
    fn begin_and_complete_now_wrappers_drive_the_clock() {
        // The production wrappers stamp the current instant; exercise them so the
        // Instant::now() seams are covered.
        let handle = TaskHandle::new();
        let id = handle.begin(TaskKind::CreateSession, "m");
        assert!(handle.is_active(Instant::now()));
        handle.complete(id, true, completion());
        assert!(handle.is_active(Instant::now()));
        assert_eq!(handle.drain_completed().len(), 1);
    }

    #[test]
    fn panic_outcome_records_a_str_payload_and_marks_the_row_failed() {
        // `panic!("…")` hands `catch_unwind` a `&str` payload.
        let payload: Box<dyn std::any::Any + Send> = Box::new("boom");
        let (log_line, completion) = panic_outcome(TaskKind::CreateSession, "alpha", payload);

        // The persisted (file-log) line names the operation, target, and the raw
        // panic message for diagnosis.
        assert_eq!(log_line, "session create \"alpha\" worker panicked: boom");
        // The row settles as a failure instead of spinning forever.
        assert_eq!(completion.line.kind, LineKind::Error);
        // The on-screen line is a plain user-facing message — the raw panic text
        // ("boom") stays in the log, not the UI.
        assert_eq!(
            completion.line.text,
            "作成が異常終了しました（alpha）。詳細はログを確認してください"
        );
        assert!(!completion.line.text.contains("boom"));
        assert!(completion.sessions.is_none());
        assert!(completion.evict.is_none());
        assert!(completion.focus.is_none());
    }

    #[test]
    fn panic_outcome_records_a_string_payload_for_a_removal() {
        // A formatted panic (`panic!("{x}")`) hands back an owned `String`.
        let payload: Box<dyn std::any::Any + Send> = Box::new(String::from("disk gone"));
        let (log_line, completion) = panic_outcome(TaskKind::RemoveSession, "beta", payload);

        assert_eq!(
            log_line,
            "session remove \"beta\" worker panicked: disk gone"
        );
        // The raw payload is kept out of the on-screen line.
        assert_eq!(
            completion.line.text,
            "削除が異常終了しました（beta）。詳細はログを確認してください"
        );
        assert!(!completion.line.text.contains("disk gone"));
    }

    #[test]
    fn panic_outcome_falls_back_for_an_opaque_payload() {
        // A non-string payload (e.g. `std::panic::panic_any(42)`) still logs.
        let payload: Box<dyn std::any::Any + Send> = Box::new(42_i32);
        let (log_line, _) = panic_outcome(TaskKind::CreateSession, "gamma", payload);

        assert_eq!(
            log_line,
            "session create \"gamma\" worker panicked: unknown panic payload"
        );
    }
}

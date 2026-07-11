//! Terminal-restoring signal handlers for the interactive TUI.
//!
//! The RAII guards ([`AlternateScreenGuard`](super::screen::AlternateScreenGuard)
//! and the embedded pane's mode guard) reset the terminal — leave the alternate
//! screen, disable mouse reporting, show the cursor — when they drop on a normal
//! return or a panic unwind. A signal that terminates the process
//! (SIGINT / SIGTERM / SIGHUP) runs no destructors, so without these handlers the
//! shell is left with mouse reporting still on: every pointer move then echoes a
//! `\x1b[<btn;x;yM` report as visible garbage. The classic trigger is Ctrl-C
//! under `cargo run` landing in the sliver of time the TUI is not holding raw
//! mode (so `console` does not translate it to `Key::CtrlC`) — a real SIGINT
//! kills usagi before any `Drop` runs — but `kill` (SIGTERM) and closing the
//! terminal / SSH session (SIGHUP) skip the guards the same way.
//!
//! [`install`] registers a handler for those three signals. It writes the shared
//! [`TERMINAL_RESTORE`](super::screen::TERMINAL_RESTORE) sequence straight to the
//! stdout fd and restores the pre-TUI line discipline, then re-raises the signal
//! so the process still dies with the signal's normal semantics. Everything the
//! handler does is async-signal-safe: a raw `write(2)`, a `tcsetattr(2)` to a
//! termios captured before the TUI started, and `raise(3)` — no allocation, no
//! lock, no Rust `Drop`.
//!
//! [`install`] also registers a **no-op SIGWINCH handler**. Its entire effect is
//! that a terminal resize now interrupts the TUI's blocking `select`/`poll` key
//! reads with `EINTR` — the wake-up the event loops turn into a full repaint at
//! the new size (`console` surfaces the interrupted read as a spurious
//! `Key::CtrlC`, which `io::term_reader` maps to `Key::Unknown`). SIGWINCH's
//! default disposition is "ignore", so without this a resize before the embedded
//! pane first ran (which installs crossterm's SIGWINCH handler as a side effect
//! of its event stream) woke nothing, and the screen stayed stale until the next
//! keypress. crossterm registers via `signal-hook`, which chains a previously
//! installed `sigaction` handler, so the two coexist.

#[cfg(unix)]
mod imp {
    use std::mem::MaybeUninit;
    use std::os::raw::c_int;
    use std::ptr::{addr_of, addr_of_mut};
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::super::screen::TERMINAL_RESTORE;

    /// The terminal mode captured before the TUI switched to raw mode. The
    /// handler restores it so the shell gets its line discipline (echo, canonical
    /// input) back on an abrupt exit. Written once by [`install`] on the main
    /// thread before any handler can fire and never mutated afterwards, so the
    /// `static mut` access races with nothing.
    static mut ORIGINAL_TERMIOS: MaybeUninit<libc::termios> = MaybeUninit::uninit();
    /// Whether [`ORIGINAL_TERMIOS`] holds a valid mode (stdin was a tty at install
    /// time). Gates the handler's `tcsetattr`.
    static TERMIOS_SAVED: AtomicBool = AtomicBool::new(false);
    /// Guards [`install`] against registering the handlers more than once.
    static INSTALLED: AtomicBool = AtomicBool::new(false);

    /// Restore the terminal, then re-raise `signum` under its default disposition.
    ///
    /// Installed with `SA_RESETHAND`, so entering the handler resets the signal to
    /// `SIG_DFL`; the trailing `raise` then terminates the process with the
    /// signal's normal semantics (correct exit status, SIGHUP propagation) instead
    /// of re-entering us. Async-signal-safe throughout: only `write`, `tcsetattr`,
    /// and `raise`.
    extern "C" fn handle(signum: c_int) {
        unsafe {
            libc::write(
                libc::STDOUT_FILENO,
                TERMINAL_RESTORE.as_ptr() as *const libc::c_void,
                TERMINAL_RESTORE.len(),
            );
            if TERMIOS_SAVED.load(Ordering::Relaxed) {
                libc::tcsetattr(
                    libc::STDIN_FILENO,
                    libc::TCSANOW,
                    addr_of!(ORIGINAL_TERMIOS) as *const libc::termios,
                );
            }
            libc::raise(signum);
        }
    }

    /// Wake blocking reads on a terminal resize. The body is empty on purpose:
    /// registering *any* handler moves SIGWINCH off its default "ignore"
    /// disposition, so a resize interrupts the TUI's blocking `select`/`poll`
    /// key reads with `EINTR` — which the key reader and event loops turn into
    /// a full repaint at the new size (see the module docs). Trivially
    /// async-signal-safe: it does nothing.
    extern "C" fn handle_winch(_signum: c_int) {}

    /// Install the terminal-restoring handler for SIGINT, SIGTERM, and SIGHUP,
    /// and the read-waking no-op handler for SIGWINCH.
    /// Idempotent: only the first call registers anything.
    pub fn install() {
        if INSTALLED.swap(true, Ordering::SeqCst) {
            return;
        }
        unsafe {
            // Capture the current (pre-raw, cooked) terminal mode so the handler
            // can put the line discipline back. Skipped when stdin is not a tty.
            if libc::tcgetattr(
                libc::STDIN_FILENO,
                addr_of_mut!(ORIGINAL_TERMIOS) as *mut libc::termios,
            ) == 0
            {
                TERMIOS_SAVED.store(true, Ordering::SeqCst);
            }

            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handle as *const libc::c_void as libc::sighandler_t;
            libc::sigemptyset(addr_of_mut!(action.sa_mask));
            // Reset to the default disposition after firing once so the handler's
            // own `raise` terminates the process rather than re-entering us.
            action.sa_flags = libc::SA_RESETHAND;
            for signum in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                libc::sigaction(signum, addr_of!(action), std::ptr::null_mut());
            }

            // SIGWINCH: registered with no flags — no `SA_RESTART`, so a resize
            // interrupts blocking reads (EINTR) and the event loops repaint at
            // the new size; no `SA_RESETHAND`, so every resize keeps firing.
            let mut winch: libc::sigaction = std::mem::zeroed();
            winch.sa_sigaction = handle_winch as *const libc::c_void as libc::sighandler_t;
            libc::sigemptyset(addr_of_mut!(winch.sa_mask));
            winch.sa_flags = 0;
            libc::sigaction(libc::SIGWINCH, addr_of!(winch), std::ptr::null_mut());
        }
    }
}

#[cfg(unix)]
pub use imp::install;

/// No terminal-terminating signals to trap on non-Unix targets: Windows uses a
/// separate console-control mechanism, and the alternate-screen guard's `Drop`
/// covers the normal exit paths there. Kept as a no-op so callers need no `cfg`.
#[cfg(not(unix))]
pub fn install() {}

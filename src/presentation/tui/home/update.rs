//! A shared handle the home event loop reads to learn when a background update
//! check has found a newer release.
//!
//! The update check (`git ls-remote` against the project remote) runs on its own
//! thread so a slow or unreachable network never blocks the screen. The thread
//! writes its result here once; the event loop reads it before each redraw and,
//! when a newer release is available, surfaces the top-right notice.

use std::sync::{Arc, Mutex};

use crate::usecase::update_check::UpdateStatus;

/// A cloneable handle onto the result of the background update check.
///
/// Cloning shares the same underlying slot, so the background thread's
/// [`set`](Self::set) is visible to every reader. A fresh handle (the default)
/// reports no update — what the screen shows until the check completes, and what
/// it shows forever when the build is already up to date.
#[derive(Clone, Default)]
pub struct UpdateHandle {
    shared: Arc<Mutex<Option<UpdateStatus>>>,
}

impl UpdateHandle {
    /// A handle reporting no update yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that a newer release was found (called from the background thread).
    pub fn set(&self, status: UpdateStatus) {
        *self.lock() = Some(status);
    }

    /// The available update, once the background check has found one; `None`
    /// while the check is pending or when the build is up to date.
    pub fn status(&self) -> Option<UpdateStatus> {
        *self.lock()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<UpdateStatus>> {
        // Recover a poisoned lock rather than propagating the panic. The home
        // event loop reads this handle before every redraw while the terminal is
        // in raw / alternate-screen mode, so escalating a poison here would crash
        // the UI with the terminal left broken. The slot only guards a `replace` /
        // read of an `Option`, so a stale reading is the worst outcome. This
        // matches the never-crash-on-poison policy of the sibling handles
        // (`SessionsRefreshHandle`, the terminal pool / monitor).
        self.shared.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::version::Version;

    fn status() -> UpdateStatus {
        UpdateStatus {
            current: Version::parse("0.0.1").unwrap(),
            latest: Version::parse("0.2.0").unwrap(),
        }
    }

    #[test]
    fn a_fresh_handle_reports_no_update() {
        assert!(UpdateHandle::new().status().is_none());
    }

    #[test]
    fn set_is_visible_through_a_clone() {
        let handle = UpdateHandle::new();
        let writer = handle.clone();
        writer.set(status());
        assert_eq!(handle.status(), Some(status()));
    }

    #[test]
    fn lock_recovers_from_a_poisoned_mutex_instead_of_crashing() {
        // A thread that panics while holding the lock poisons the mutex. The
        // handle must still hand back the last-written value rather than
        // propagating the poison and crashing the TUI event loop that reads it.
        let handle = UpdateHandle::new();
        handle.set(status());
        let clone = handle.clone();
        let _ = std::thread::spawn(move || {
            let _guard = clone.shared.lock().unwrap_or_else(|e| e.into_inner());
            panic!("poison the mutex");
        })
        .join();
        assert_eq!(handle.status(), Some(status()));
    }
}

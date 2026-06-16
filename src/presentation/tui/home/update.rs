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
        self.shared.lock().expect("update handle mutex poisoned")
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
}

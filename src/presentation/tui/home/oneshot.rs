//! A cloneable one-shot slot a background startup probe fills and the event loop
//! drains exactly once.
//!
//! The home screen kicks off slow startup work — probing installed Agent CLIs and
//! re-syncing the worktree statuses from git — on background threads so neither
//! delays the first paint. Each thread writes its result here once; the event loop
//! [`take`](OneShot::take)s it on a later frame and applies it, so the screen
//! opens immediately and updates in place when the result lands. Mirrors
//! [`UpdateHandle`](super::update::UpdateHandle), generalised over the payload
//! type.

use std::sync::{Arc, Mutex};

/// A cloneable handle onto a value produced once by a background thread.
///
/// Cloning shares the same slot, so the producer's [`set`](Self::set) is visible
/// to the reader. A fresh handle (the default) holds nothing — what the screen
/// sees until the background work completes.
#[derive(Clone)]
pub struct OneShot<T> {
    shared: Arc<Mutex<Option<T>>>,
}

impl<T> Default for OneShot<T> {
    fn default() -> Self {
        Self {
            shared: Arc::new(Mutex::new(None)),
        }
    }
}

impl<T> OneShot<T> {
    /// An empty handle, holding no value yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the produced value (called from the background thread). A later
    /// [`take`](Self::take) hands it to the event loop. Replaces any unread
    /// value, so the most recent result wins.
    pub fn set(&self, value: T) {
        *self.lock() = Some(value);
    }

    /// Remove and return the value if one has been produced, leaving the slot
    /// empty so it is applied only once; `None` while the work is still pending
    /// or after it has already been drained.
    pub fn take(&self) -> Option<T> {
        self.lock().take()
    }

    /// Recovers the guard rather than panicking if the lock was poisoned: this is
    /// read on the render path, and a panicked producer thread must not take the
    /// whole TUI down. A missed startup result beats a crash that leaves the
    /// terminal in raw mode. Mirrors the pool / monitor poison handling.
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<T>> {
        self.shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_handle_holds_nothing() {
        let slot: OneShot<u32> = OneShot::new();
        assert_eq!(slot.take(), None);
    }

    #[test]
    fn set_is_visible_through_a_clone_and_taken_once() {
        let slot: OneShot<u32> = OneShot::default();
        let producer = slot.clone();
        producer.set(7);
        // The reader drains it exactly once; a second take finds it empty.
        assert_eq!(slot.take(), Some(7));
        assert_eq!(slot.take(), None);
    }

    #[test]
    fn set_replaces_an_unread_value() {
        let slot: OneShot<u32> = OneShot::new();
        slot.set(1);
        slot.set(2);
        assert_eq!(slot.take(), Some(2));
    }

    #[test]
    fn take_recovers_from_a_poisoned_lock() {
        use std::sync::Arc;
        let slot: OneShot<u32> = OneShot::new();
        slot.set(5);
        let clone = slot.clone();
        // Poison the mutex from a panicking thread, then confirm the value is
        // still readable rather than the read panicking.
        let _ = std::thread::spawn(move || {
            let _guard = clone.lock();
            panic!("poison");
        })
        .join();
        assert_eq!(Arc::strong_count(&slot.shared), 1);
        assert_eq!(slot.take(), Some(5));
    }
}

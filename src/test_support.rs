//! Helpers shared by tests that touch process-global state.

use std::sync::{Mutex, MutexGuard, OnceLock};

/// Serializes tests that read or write process-global state — the current
/// working directory and the `$USAGI_HOME` override — which cargo's parallel
/// test runner would otherwise let race against each other.
///
/// Hold the returned guard for the duration of the mutation (and any reads that
/// depend on it). A test that panics while holding the guard poisons it, which
/// surfaces as panics in the other guarded tests — acceptable, since the suite
/// has already failed at that point.
pub fn process_env_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
}

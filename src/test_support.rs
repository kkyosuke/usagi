//! Helpers shared by tests that touch process-global state.

use std::sync::{Mutex, MutexGuard, OnceLock};

/// Serializes tests that read or write process-global state — the current
/// working directory and the `$USAGI_HOME` override — which cargo's parallel
/// test runner would otherwise let race against each other.
///
/// Hold the returned guard for the duration of the mutation (and any reads that
/// depend on it). If a failing test poisons the mutex, later guarded tests recover
/// the inner lock rather than cascading the original panic into unrelated tests.
pub fn process_env_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

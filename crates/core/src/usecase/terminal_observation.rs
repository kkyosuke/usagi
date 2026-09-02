//! Shared hard bounds for read-only terminal observation surfaces.

/// Default number of retained terminal rows returned to an observer.
pub const TERMINAL_READ_DEFAULT_LINES: usize = 200;
/// Largest caller-selected terminal row tail.
pub const TERMINAL_READ_MAX_LINES: usize = 500;
/// Absolute UTF-8 response-content bound after the row bound is applied.
pub const TERMINAL_READ_MAX_BYTES: usize = 64 * 1024;

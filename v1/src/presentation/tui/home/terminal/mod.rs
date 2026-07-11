//! The home screen's embedded terminal: PTY-backed panes and everything around them.
//!
//! Acting on a worktree opens an interactive terminal pane inside the home
//! screen. This module groups the pieces that make that work:
//!
//! - [`pool`] — the per-worktree pool of PTY-backed panes and their monitors.
//! - [`pane`] — the interactive drive loop for a single pane (coverage-excluded IO).
//! - [`view`] — the immutable grid-to-rows snapshot the UI renders.
//! - [`tabs`] — the agent/terminal tab strip and its navigation logic.
//! - [`selection`] — drag-selection geometry over the rendered grid.
//! - [`link`] — `http(s)` link detection within the rendered grid.

pub mod link;
pub mod pane;
pub mod pool;
pub mod selection;
pub mod tabs;
pub mod view;

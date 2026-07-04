//! Low-level terminal IO plumbing shared by every TUI screen.
//!
//! The screens themselves live one level up; this module holds the pieces that
//! talk to the real terminal device rather than drawing any one screen:
//!
//! - [`screen`] — alternate-screen / raw-mode / mouse-reporting setup and the
//!   shared frame-painting helpers.
//! - [`term_reader`] — reading keys and byte sequences from stdin with timeouts.
//! - [`echo`] — suppressing terminal echo for the TUI's lifetime.
//! - [`clipboard`] — copying selected text to the system clipboard.
//! - [`loading`] — running a slow blocking step on a worker thread while the
//!   screen animates a loading rabbit.

pub mod clipboard;
pub mod echo;
pub mod loading;
pub mod screen;
pub mod term_reader;

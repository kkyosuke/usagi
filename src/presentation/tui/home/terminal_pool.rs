//! A pool of live embedded terminals, one per worktree directory.
//!
//! The workspace screen embeds at most one shell per worktree in its right
//! pane. To let the user switch sessions while a `terminal` or `agent` keeps
//! running, the [`PtySession`]s cannot live on the stack of the terminal loop
//! (where leaving would drop — and so kill — them). Instead they are owned here,
//! keyed by worktree directory, for the lifetime of the screen: detaching
//! (`Ctrl-O`) returns to the sidebar but leaves the shell — and any agent CLI
//! running inside it — alive in the pool, so re-attaching later finds it exactly
//! where it was left.
//!
//! This is pure I/O and process ownership (it spawns shells and holds their
//! handles), so — like [`PtySession`] itself — it is excluded from coverage. The
//! geometry it spawns at ([`super::ui::terminal_geometry`]) is tested on its own.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use console::Term;

use crate::infrastructure::pty::PtySession;

use super::ui;

/// The live shells embedded in the workspace screen, keyed by worktree path.
///
/// Owned by the screen ([`super::run`]); dropped when the user leaves it, which
/// kills every shell it still holds (via [`PtySession`]'s `Drop`).
pub struct TerminalPool {
    sessions: HashMap<PathBuf, PtySession>,
}

impl TerminalPool {
    /// An empty pool.
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }

    /// Borrow the live shell rooted at `dir`, spawning one if none exists yet
    /// (or the previous one has exited). On a fresh spawn the `initial` command
    /// line is sent once — this is how `agent` lands the user in the configured
    /// agent CLI; re-attaching to an existing shell never re-sends it.
    ///
    /// The shell is sized to the right pane's current geometry; the terminal
    /// loop resizes it from then on as the window changes.
    pub fn attach_or_spawn(
        &mut self,
        term: &Term,
        dir: &Path,
        initial: Option<&str>,
    ) -> Result<&mut PtySession> {
        let key = dir.to_path_buf();
        let alive = self.sessions.get(&key).is_some_and(|s| s.is_alive());
        if !alive {
            let (height, width) = term.size();
            let geo = ui::terminal_geometry(height as usize, width as usize);
            let mut pty = PtySession::spawn(dir, geo.rows, geo.cols)?;
            if let Some(command) = initial {
                // The shell buffers its input, so writing immediately is fine:
                // it runs the command once it has started up.
                pty.write(format!("{command}\r").as_bytes())?;
            }
            // Overwrites (and so drops/kills) any exited shell at this path.
            self.sessions.insert(key.clone(), pty);
        }
        Ok(self
            .sessions
            .get_mut(&key)
            .expect("the session was just inserted or already present"))
    }
}

impl Default for TerminalPool {
    fn default() -> Self {
        Self::new()
    }
}

use anyhow::Result;
use console::Key;
use console::Term;

use crate::presentation::tui::screen::KeyReader;

use super::state::WorktreeList;
use super::ui;

/// What the user chose to do on the home (workspace) screen.
#[derive(Debug)]
pub enum Outcome {
    /// Return to the project selection screen without acting on a worktree.
    Back,
    /// The user asked to quit the application entirely.
    Quit,
}

/// Runs the home screen against the given terminal and key source until the
/// user goes back or quits. Assumes the alternate screen is already active (it
/// is owned by the launch screen, several levels up).
///
/// Opening a worktree is a placeholder for now: selecting one shows a "coming
/// soon" notice, since the per-worktree session screen is not implemented yet.
pub fn event_loop(
    term: &Term,
    reader: &mut dyn KeyReader,
    mut list: WorktreeList,
    initial_notice: Option<String>,
) -> Result<Outcome> {
    let mut notice = initial_notice;

    loop {
        term.move_cursor_to(0, 0)?;
        term.clear_screen()?;
        let (height, width) = term.size();
        let frame = ui::render_frame(height as usize, width as usize, &list, notice.as_deref());
        for line in &frame {
            term.write_line(line)?;
        }

        let key = match reader.read_key() {
            Ok(key) => key,
            // An interrupted read (e.g. a delivered signal) means quit.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return Ok(Outcome::Quit),
            Err(e) => return Err(anyhow::Error::from(e).context("Failed to read key")),
        };

        match key {
            Key::ArrowUp | Key::Char('k') => {
                list.move_up();
                notice = None;
            }
            Key::ArrowDown | Key::Char('j') => {
                list.move_down();
                notice = None;
            }
            Key::Enter => {
                if let Some(worktree) = list.selected() {
                    let branch = worktree.branch.as_deref().unwrap_or("(detached)");
                    notice = Some(format!("Opening \"{branch}\" is coming soon 🐰"));
                }
            }
            Key::Char('q') | Key::Escape => return Ok(Outcome::Back),
            Key::CtrlC => return Ok(Outcome::Quit),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::workspace_state::{BranchStatus, WorktreeState};
    use chrono::Utc;
    use std::collections::VecDeque;
    use std::io;
    use std::path::PathBuf;

    /// A key source that replays a scripted sequence of results.
    struct ScriptedReader {
        keys: VecDeque<io::Result<Key>>,
    }

    impl ScriptedReader {
        fn new(keys: Vec<io::Result<Key>>) -> Self {
            Self { keys: keys.into() }
        }
    }

    impl KeyReader for ScriptedReader {
        fn read_key(&mut self) -> io::Result<Key> {
            // Default to Escape so a test can never spin forever.
            self.keys.pop_front().unwrap_or(Ok(Key::Escape))
        }
    }

    fn worktree(branch: Option<&str>) -> WorktreeState {
        WorktreeState {
            branch: branch.map(|b| b.to_string()),
            path: PathBuf::from("/repo/wt"),
            head: "abc1234".to_string(),
            primary: false,
            upstream: None,
            status: BranchStatus::Local,
            updated_at: Utc::now(),
        }
    }

    fn sample_list() -> WorktreeList {
        WorktreeList::new(
            "usagi",
            vec![worktree(Some("main")), worktree(Some("feature"))],
        )
    }

    fn run(keys: Vec<io::Result<Key>>, list: WorktreeList) -> Result<Outcome> {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(keys);
        event_loop(&term, &mut reader, list, None)
    }

    #[test]
    fn escape_returns_back() {
        assert!(matches!(
            run(vec![Ok(Key::Escape)], sample_list()).unwrap(),
            Outcome::Back
        ));
    }

    #[test]
    fn q_returns_back() {
        assert!(matches!(
            run(vec![Ok(Key::Char('q'))], sample_list()).unwrap(),
            Outcome::Back
        ));
    }

    #[test]
    fn ctrl_c_returns_quit() {
        assert!(matches!(
            run(vec![Ok(Key::CtrlC)], sample_list()).unwrap(),
            Outcome::Quit
        ));
    }

    #[test]
    fn navigation_keys_move_the_cursor_then_back() {
        // Exercises every navigation arm (arrows + j/k aliases) and the
        // ignored-key arm, then leaves via Escape.
        let keys = vec![
            Ok(Key::ArrowDown),
            Ok(Key::ArrowUp),
            Ok(Key::Char('j')),
            Ok(Key::Char('k')),
            Ok(Key::Home), // ignored (the `_` arm)
            Ok(Key::Escape),
        ];
        assert!(matches!(run(keys, sample_list()).unwrap(), Outcome::Back));
    }

    #[test]
    fn enter_on_a_worktree_shows_a_notice_then_back() {
        // Enter selects a worktree (sets the "coming soon" notice), then Escape.
        let keys = vec![Ok(Key::Enter), Ok(Key::Escape)];
        assert!(matches!(run(keys, sample_list()).unwrap(), Outcome::Back));
    }

    #[test]
    fn enter_on_detached_worktree_shows_a_notice() {
        // A detached HEAD has no branch name; Enter still produces a notice.
        let keys = vec![Ok(Key::Enter), Ok(Key::Escape)];
        let list = WorktreeList::new("usagi", vec![worktree(None)]);
        assert!(matches!(run(keys, list).unwrap(), Outcome::Back));
    }

    #[test]
    fn enter_on_empty_list_does_nothing() {
        // With no worktrees there is nothing to select; Enter is a no-op.
        let keys = vec![Ok(Key::Enter), Ok(Key::Escape)];
        let list = WorktreeList::new("usagi", Vec::new());
        assert!(matches!(run(keys, list).unwrap(), Outcome::Back));
    }

    #[test]
    fn initial_notice_is_displayed() {
        // A load-error notice passed in is rendered on the first frame.
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Escape)]);
        let outcome = event_loop(
            &term,
            &mut reader,
            WorktreeList::new("usagi", Vec::new()),
            Some("Failed to load worktrees: boom".to_string()),
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Back));
    }

    #[test]
    fn interrupted_read_returns_quit() {
        let keys = vec![Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "interrupted",
        ))];
        assert!(matches!(run(keys, sample_list()).unwrap(), Outcome::Quit));
    }

    #[test]
    fn unexpected_read_error_is_propagated() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Err(io::Error::other("boom"))]);
        let err = event_loop(&term, &mut reader, sample_list(), None).unwrap_err();
        assert!(err.to_string().contains("Failed to read key"));
    }
}

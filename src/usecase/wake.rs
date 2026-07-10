//! The wake broadcast: send `continue` to every session whose agent pane is live.
//!
//! When a [`WakeSchedule`](crate::domain::wake::WakeSchedule) fires, the home
//! screen resumes every *running* session agent at once by enqueueing `continue`
//! for live delivery — the same live channel the MCP `session_prompt` tool uses,
//! so a running usagi TUI types it into each session's agent pane and presses
//! Enter. This module holds the pure selection core: given the session roots and
//! injected "is this session's agent live?" / "enqueue for this session" hooks, it
//! messages only the live ones and reports how many. The real hooks
//! (`agent_live_pane_store` / `agent_live_prompt_store`) are wired at the call
//! site so this stays unit-tested without a filesystem or a live pane.

use std::path::Path;

/// The text sent to each running agent when a wake fires.
pub const CONTINUE_PROMPT: &str = "continue";

/// Enqueue `continue` for every session root in `roots` that currently has a live
/// agent pane, returning how many agents were messaged.
///
/// `is_live` reports whether a session root has a live agent pane (production: the
/// live-pane marker), and `append` enqueues the prompt for live delivery
/// (production: the live-prompt store). Both are injected so the selection logic
/// is testable without the filesystem or a running TUI. A session with no live
/// pane is skipped (nothing waits on a queue no one drains), and a failed append
/// is skipped rather than aborting the batch, so one wedged session never blocks
/// the rest from resuming.
pub fn broadcast_continue<'a>(
    roots: impl IntoIterator<Item = &'a Path>,
    is_live: impl Fn(&Path) -> bool,
    mut append: impl FnMut(&Path) -> Result<(), String>,
) -> usize {
    let mut sent = 0;
    for root in roots {
        if is_live(root) && append(root).is_ok() {
            sent += 1;
        }
    }
    sent
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::PathBuf;

    #[test]
    fn messages_only_live_sessions_and_counts_them() {
        let roots = [
            PathBuf::from("/a"),
            PathBuf::from("/b"),
            PathBuf::from("/c"),
        ];
        let sent_to = RefCell::new(Vec::new());
        // Only /a and /c are live.
        let count = broadcast_continue(
            roots.iter().map(|p| p.as_path()),
            |p| p != Path::new("/b"),
            |p| {
                sent_to.borrow_mut().push(p.to_path_buf());
                Ok(())
            },
        );
        assert_eq!(count, 2);
        assert_eq!(
            sent_to.into_inner(),
            vec![PathBuf::from("/a"), PathBuf::from("/c")]
        );
    }

    #[test]
    fn a_failed_append_is_skipped_not_counted() {
        let roots = [PathBuf::from("/a"), PathBuf::from("/b")];
        // Both live, but enqueueing /a fails; only /b counts.
        let count = broadcast_continue(
            roots.iter().map(|p| p.as_path()),
            |_| true,
            |p| {
                if p == Path::new("/a") {
                    Err("disk full".to_string())
                } else {
                    Ok(())
                }
            },
        );
        assert_eq!(count, 1);
    }

    #[test]
    fn no_live_sessions_messages_no_one() {
        let roots = [PathBuf::from("/a")];
        fn append(_: &Path) -> Result<(), String> {
            Ok(())
        }
        let count = broadcast_continue(roots.iter().map(|p| p.as_path()), |_| false, append);
        assert_eq!(count, 0);
        let _ = append(Path::new("/"));
    }
}

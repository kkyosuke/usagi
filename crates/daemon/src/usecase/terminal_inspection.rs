//! Bounded, read-only projection of a daemon-owned terminal snapshot.
//!
//! MCP and future observation surfaces consume the semantic screen checkpoint,
//! never raw PTY bytes. This keeps VT control sequences out of model input and
//! makes one hard response bound apply regardless of terminal history size.

use serde::Serialize;
use usagi_core::{
    domain::id::TerminalId,
    usecase::{
        terminal_observation::{TERMINAL_READ_MAX_BYTES, TERMINAL_READ_MAX_LINES},
        vt_screen::{CheckpointError, VtScreen},
    },
};

use super::terminal::Snapshot;

/// Plain-text terminal observation returned to an authenticated caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TerminalInspection {
    pub terminal_id: TerminalId,
    pub live: bool,
    pub exit_code: Option<i32>,
    pub output_offset: u64,
    pub content: String,
    pub returned_lines: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalInspectionError {
    InvalidLineLimit,
    InvalidCheckpoint(CheckpointError),
}

/// Converts one atomic semantic checkpoint to a bounded ANSI-free tail.
///
/// # Errors
///
/// Returns [`TerminalInspectionError::InvalidLineLimit`] outside the public
/// line bound, or [`TerminalInspectionError::InvalidCheckpoint`] when the
/// daemon-owned checkpoint cannot be reconstructed safely.
pub fn inspect_terminal(
    snapshot: Snapshot,
    line_limit: usize,
) -> Result<TerminalInspection, TerminalInspectionError> {
    if !(1..=TERMINAL_READ_MAX_LINES).contains(&line_limit) {
        return Err(TerminalInspectionError::InvalidLineLimit);
    }
    let Snapshot {
        terminal,
        output_offset,
        screen,
        exited,
        ..
    } = snapshot;
    let screen =
        VtScreen::from_checkpoint(&screen).map_err(TerminalInspectionError::InvalidCheckpoint)?;
    let mut rows = screen
        .cells_with_scrollback()
        .into_iter()
        // A restored checkpoint materializes every grid cell, including the
        // blank padding to the right edge. Observation is plain terminal text,
        // not a rectangular selection, so that padding is not content.
        .map(|row| row.trim_end_matches(' ').to_owned())
        .collect::<Vec<_>>();
    while rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    let wraps = screen.soft_wraps_with_scrollback();
    let row_truncated = rows.len() > line_limit || screen.scrollback_origin() > 0;
    let start = rows.len().saturating_sub(line_limit);
    let mut content = String::new();
    for (index, row) in rows.iter().enumerate().skip(start) {
        content.push_str(row);
        if index + 1 < rows.len() && !wraps[index] {
            content.push('\n');
        }
    }
    let (content, byte_truncated) = bounded_tail(content, TERMINAL_READ_MAX_BYTES);
    let returned_lines = if content.is_empty() {
        0
    } else {
        content.lines().count()
    };
    Ok(TerminalInspection {
        terminal_id: terminal.terminal_id,
        live: exited.is_none(),
        exit_code: exited,
        output_offset,
        content,
        returned_lines,
        truncated: row_truncated || byte_truncated,
    })
}

fn bounded_tail(content: String, maximum: usize) -> (String, bool) {
    if content.len() <= maximum {
        return (content, false);
    }
    let mut start = content.len() - maximum;
    while !content.is_char_boundary(start) {
        start += 1;
    }
    (content[start..].to_owned(), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usecase::terminal::{Geometry, Snapshot};
    use usagi_core::{
        domain::id::{
            DaemonGeneration, SessionId, TerminalId, TerminalRef, WorkspaceId, WorktreeId,
        },
        usecase::vt_screen::VtScreen,
    };

    fn terminal() -> TerminalRef {
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: WorkspaceId::new(),
            session_id: Some(SessionId::new()),
            worktree_id: WorktreeId::new(),
        }
    }

    fn snapshot(screen: &VtScreen, exited: Option<i32>) -> Snapshot {
        let checkpoint = screen.checkpoint();
        Snapshot {
            terminal: terminal(),
            revision: 1,
            base_offset: 0,
            output_offset: 42,
            geometry: Geometry {
                cols: u16::try_from(checkpoint.geometry.cols).unwrap(),
                rows: u16::try_from(checkpoint.geometry.rows).unwrap(),
            },
            replay: Vec::new(),
            screen: Box::new(checkpoint),
            exited,
        }
    }

    #[test]
    fn semantic_snapshot_becomes_ansi_free_plain_text() {
        let mut screen = VtScreen::new(3, 40);
        screen.advance(b"$ cargo test\r\n\x1b[31merror: failed\x1b[0m");
        let observed = inspect_terminal(snapshot(&screen, Some(1)), 20).unwrap();
        assert_eq!(observed.content, "$ cargo test\nerror: failed");
        assert!(!observed.content.contains('\u{1b}'));
        assert!(!observed.live);
        assert_eq!(observed.exit_code, Some(1));
        assert_eq!(observed.output_offset, 42);
        assert_eq!(observed.returned_lines, 2);
        assert!(!observed.truncated);
    }

    #[test]
    fn line_and_utf8_byte_bounds_keep_the_newest_tail() {
        let mut screen = VtScreen::new(6, 20);
        screen.advance(b"one\r\ntwo\r\nthree\r\nfour");
        let observed = inspect_terminal(snapshot(&screen, None), 2).unwrap();
        assert_eq!(observed.content, "three\nfour");
        assert!(observed.live);
        assert!(observed.truncated);

        let unicode = "あ".repeat(TERMINAL_READ_MAX_BYTES);
        let (tail, truncated) = bounded_tail(unicode, TERMINAL_READ_MAX_BYTES);
        assert!(truncated);
        assert!(tail.len() <= TERMINAL_READ_MAX_BYTES);
        assert!(tail.is_char_boundary(0));
    }

    #[test]
    fn soft_wrapped_rows_are_returned_as_one_plain_text_line() {
        let mut screen = VtScreen::new(2, 4);
        screen.advance(b"abcdef");
        let observed = inspect_terminal(snapshot(&screen, None), 2).unwrap();
        assert_eq!(observed.content, "abcdef");
        assert_eq!(observed.returned_lines, 1);
    }

    #[test]
    fn invalid_limits_and_checkpoints_fail_closed() {
        let screen = VtScreen::new(1, 1);
        assert_eq!(
            inspect_terminal(snapshot(&screen, None), 0).unwrap_err(),
            TerminalInspectionError::InvalidLineLimit
        );
        assert_eq!(
            inspect_terminal(snapshot(&screen, None), TERMINAL_READ_MAX_LINES + 1).unwrap_err(),
            TerminalInspectionError::InvalidLineLimit
        );
        let mut invalid = snapshot(&screen, None);
        invalid.screen.schema_version = u16::MAX;
        assert!(matches!(
            inspect_terminal(invalid, 1),
            Err(TerminalInspectionError::InvalidCheckpoint(_))
        ));

        let mut history_truncated = snapshot(&screen, None);
        history_truncated.screen.primary.scrollback_origin = 1;
        assert!(inspect_terminal(history_truncated, 1).unwrap().truncated);
    }
}

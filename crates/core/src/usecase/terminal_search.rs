//! Bounded search of semantic terminal cells, shared by the daemon and TUI.

use super::vt_screen::{ActiveBuffer, Geometry, ScreenCheckpoint, VtScreen};
use crate::domain::id::{TerminalRef, WorkspaceId};
use serde::{Deserialize, Serialize};

pub const QUERY_MAX: usize = 256;
pub const TERMINALS_MAX: usize = 64;
pub const MATCHES_MAX: usize = 100;
pub const ROWS_MAX: usize = 20_000;
pub const BYTES_MAX: usize = 2 * 1024 * 1024;
const CONTEXT_MAX: usize = 240;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchTerminalKind {
    Agent,
    Terminal,
}

/// An exact snapshot location. Changes after observation never retarget a hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputMatch {
    pub row: usize,
    pub column: usize,
    pub end_column: usize,
    pub before: String,
    pub text: String,
    pub after: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSearchResult {
    pub terminal: TerminalRef,
    pub kind: SearchTerminalKind,
    pub output_offset: u64,
    pub geometry: Geometry,
    pub buffer: ActiveBuffer,
    pub origin: u64,
    pub retained_rows: usize,
    pub scanned_rows: usize,
    pub exited: bool,
    pub limited: bool,
    pub unavailable: bool,
    pub matches: Vec<OutputMatch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceOutputSearch {
    pub workspace: WorkspaceId,
    pub query: String,
    pub terminals: Vec<TerminalSearchResult>,
    pub omitted_terminals: usize,
}

/// One workspace request has a shared work and response budget.
#[derive(Debug, Default)]
pub struct SearchBudget {
    rows: usize,
    bytes: usize,
    matches: usize,
}

impl SearchBudget {
    #[must_use]
    pub const fn exhausted(&self) -> bool {
        self.rows >= ROWS_MAX || self.bytes >= BYTES_MAX || self.matches >= MATCHES_MAX
    }
}

#[must_use]
pub fn valid_query(query: &str) -> bool {
    !query.is_empty() && query.chars().count() <= QUERY_MAX && !query.chars().any(char::is_control)
}

impl TerminalSearchResult {
    #[must_use]
    pub fn unavailable(terminal: TerminalRef, kind: SearchTerminalKind) -> Self {
        Self {
            terminal,
            kind,
            output_offset: 0,
            geometry: Geometry { rows: 0, cols: 0 },
            buffer: ActiveBuffer::Primary,
            origin: 0,
            retained_rows: 0,
            scanned_rows: 0,
            exited: false,
            limited: false,
            unavailable: true,
            matches: Vec::new(),
        }
    }

    /// Search the rendered cells. Escape sequences and hidden OSC payloads are
    /// absent, while columns remain terminal-cell coordinates (including CJK).
    pub fn search(
        &mut self,
        checkpoint: &ScreenCheckpoint,
        query: &str,
        budget: &mut SearchBudget,
    ) {
        let Ok(screen) = VtScreen::from_checkpoint(checkpoint) else {
            return;
        };
        if !valid_query(query) {
            return;
        }
        self.unavailable = false;
        self.geometry = checkpoint.geometry;
        self.buffer = screen.active_buffer();
        self.origin = screen.scrollback_origin();
        self.retained_rows = screen.scrollback_len() + screen.grid().len();
        self.limited = self.origin > 0;
        // A terminal's semantic screen is bounded by the VT checkpoint contract.
        // Walk cells without materializing its entire retained transcript.
        for row in 0..self.retained_rows {
            let cells = cells(&screen, row);
            let bytes = cells.iter().map(|cell| cell.ch().len_utf8()).sum::<usize>();
            if budget.exhausted() || budget.bytes.saturating_add(bytes) > BYTES_MAX {
                self.limited = true;
                break;
            }
            budget.rows += 1;
            budget.bytes += bytes;
            self.scanned_rows += 1;
            let text = row_text(&screen, row);
            for (byte, found) in text.match_indices(query) {
                if budget.matches >= MATCHES_MAX {
                    self.limited = true;
                    break;
                }
                let column = unicode_width::UnicodeWidthStr::width(&text[..byte]);
                let width = unicode_width::UnicodeWidthStr::width(found);
                self.matches.push(OutputMatch {
                    row,
                    column,
                    end_column: column.saturating_add(width.saturating_sub(1)),
                    before: row
                        .checked_sub(1)
                        .map_or_else(String::new, |row| context(&row_text(&screen, row))),
                    text: match_context(&text, byte, found.len()),
                    after: if row + 1 < self.retained_rows {
                        context(&row_text(&screen, row + 1))
                    } else {
                        String::new()
                    },
                });
                budget.matches += 1;
            }
        }
    }
}

fn cells(screen: &VtScreen, row: usize) -> &[super::vt_screen::Cell] {
    screen
        .scrollback_row(row)
        .unwrap_or_else(|| &screen.grid()[row - screen.scrollback_len()])
}

fn row_text(screen: &VtScreen, row: usize) -> String {
    cells(screen, row)
        .iter()
        .filter(|cell| !cell.continuation())
        .map(super::vt_screen::Cell::ch)
        .collect::<String>()
        .trim_end_matches(' ')
        .to_owned()
}

fn match_context(text: &str, byte: usize, len: usize) -> String {
    let before = text[..byte].chars().rev().take(80).collect::<Vec<_>>();
    let after = &text[byte + len..];
    format!(
        "{}{}{}{}{}",
        if text[..byte].chars().count() > 80 {
            "…"
        } else {
            ""
        },
        before.into_iter().rev().collect::<String>(),
        &text[byte..byte + len],
        after.chars().take(80).collect::<String>(),
        if after.chars().count() > 80 {
            "…"
        } else {
            ""
        }
    )
}

fn context(text: &str) -> String {
    text.chars().take(CONTEXT_MAX).collect()
}

#[cfg(test)]
mod tests;

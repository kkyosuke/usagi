//! Selection of the visible decision body, fenced from the covered terminal.

use usagi_core::domain::id::UserDecisionId;
use usagi_core::domain::presentation_text::sanitize_presentation_line;

use super::theme::Style;
use super::widgets::live_terminal::ViewportGeometry;
use super::widgets::{self, modal};
use super::{Key, PointerKind, Terminal};
use crate::usecase::application::controller::DecisionOverlayState;
use crate::usecase::application::terminal_selection::{
    TerminalPoint, TerminalSelection, whole_grapheme_columns,
};

const RESET: &str = "\u{1b}[0m";

#[derive(Default)]
pub(super) struct DecisionTextSelection {
    surface: Option<(Option<UserDecisionId>, bool, ViewportGeometry)>,
    rows: Vec<String>,
    press: Option<TerminalSelection>,
    selection: Option<TerminalSelection>,
    dragging: bool,
    feedback: Option<String>,
    revision: u64,
}

impl DecisionTextSelection {
    /// Snapshot only modal content. Changes to the underlying Home never clear
    /// a selection; a different decision, viewport, or edited body does.
    pub(super) fn sync(
        &mut self,
        frame: &[String],
        height: usize,
        width: usize,
        overlay: Option<&DecisionOverlayState>,
    ) {
        let surface = overlay.and_then(|overlay| {
            super::views::decision_modal::body_viewport(height, width).map(|viewport| {
                (
                    overlay.editor().map(|editor| editor.decision().decision_id),
                    overlay
                        .editor()
                        .is_some_and(|editor| editor.confirmation().is_some()),
                    viewport,
                )
            })
        });
        let rows = surface.map_or_else(Vec::new, |(_, _, viewport)| {
            frame
                .iter()
                .skip(viewport.top)
                .take(viewport.rows)
                .map(|line| {
                    widgets::strip_ansi(&modal::columns(line, viewport.left, viewport.cols))
                })
                .collect()
        });
        if self.surface != surface || self.rows != rows {
            self.surface = surface;
            self.rows = rows;
            self.press = None;
            self.selection = None;
            self.dragging = false;
            self.feedback = None;
            self.revision = self.revision.wrapping_add(1);
        }
    }

    pub(super) const fn revision(&self) -> u64 {
        self.revision
    }

    /// Own every pointer/copy event while the modal is visible. Other keys
    /// continue to its answer editor, including ordinary bracketed paste.
    pub(super) fn handle_input(&mut self, key: &Key, term: &mut dyn Terminal) -> bool {
        let Some((_, _, viewport)) = self.surface else {
            return false;
        };
        let (kind, column, row) = match key {
            Key::Click { column, row } => (PointerKind::Down, *column, *row),
            Key::Pointer(pointer) => (pointer.kind, pointer.column, pointer.row),
            Key::TerminalCopy { .. } => {
                self.copy(term);
                return true;
            }
            _ => return false,
        };
        let column = usize::from(column);
        let row = usize::from(row);
        let inside = column >= viewport.left
            && column < viewport.left + viewport.cols
            && row >= viewport.top
            && row < viewport.top + viewport.rows;
        let point = TerminalPoint {
            column: column
                .saturating_sub(viewport.left)
                .min(viewport.cols.saturating_sub(1)),
            row: row
                .saturating_sub(viewport.top)
                .min(viewport.rows.saturating_sub(1)),
        };
        match kind {
            PointerKind::Down => {
                self.press = inside.then(|| TerminalSelection::begin(self.rows.clone(), point));
                self.selection = None;
                self.dragging = false;
                self.feedback = None;
            }
            PointerKind::Drag => {
                if let Some(press) = self.press.take() {
                    self.selection = Some(press);
                    self.dragging = true;
                }
                if self.dragging {
                    self.selection
                        .as_mut()
                        .expect("drag owns selection")
                        .extend(point);
                }
            }
            PointerKind::Up => {
                self.press = None;
                if self.dragging {
                    self.selection
                        .as_mut()
                        .expect("drag owns selection")
                        .extend(point);
                    self.dragging = false;
                    self.copy(term);
                }
            }
            PointerKind::Move => return true,
        }
        self.revision = self.revision.wrapping_add(1);
        true
    }

    fn copy(&mut self, term: &mut dyn Terminal) {
        let text = self
            .selection
            .as_ref()
            .map_or_else(String::new, TerminalSelection::text);
        self.feedback = Some(if text.trim().is_empty() {
            "no decision text is selected".to_owned()
        } else {
            match term.copy_text(&text) {
                Ok(()) => format!("copied {} line(s)", text.lines().count()),
                Err(error) => sanitize_presentation_line(&error),
            }
        });
        self.revision = self.revision.wrapping_add(1);
    }

    pub(super) fn paint(&self, frame: &mut [String], width: usize) {
        let Some((_, _, viewport)) = self.surface else {
            return;
        };
        if let Some(selection) = &self.selection {
            for row in 0..viewport.rows {
                if let Some((first, last)) = selection.visual_columns_at(row) {
                    let line = &mut frame[viewport.top + row];
                    let body = modal::columns(line, viewport.left, viewport.cols);
                    let (first, end) =
                        whole_grapheme_columns(&self.rows[row], first, last, viewport.cols);
                    let selected = widgets::strip_ansi(&modal::columns(&body, first, end - first));
                    let content = format!(
                        "{}{RESET}{}{}",
                        modal::columns(&body, 0, first),
                        Style::new().reverse().paint(&selected),
                        modal::columns(&body, end, viewport.cols - end),
                    );
                    replace_body(line, &content, viewport, width);
                }
            }
        }
        // The box already has a breathing row below its body. Keep feedback
        // there so it cannot replace selected text, even in a one-row viewport.
        if let Some(feedback) = &self.feedback
            && let Some(line) = frame.get_mut(viewport.top + viewport.rows)
        {
            replace_body(line, &modal::footer(feedback), viewport, width);
        }
    }
}

fn replace_body(line: &mut String, body: &str, viewport: ViewportGeometry, width: usize) {
    let right = viewport.left + viewport.cols;
    *line = format!(
        "{}{RESET}{}{RESET}{}",
        modal::columns(line, 0, viewport.left),
        modal::columns(body, 0, viewport.cols),
        modal::columns(line, right, width.saturating_sub(right)),
    );
}

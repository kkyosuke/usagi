//! Shared live-terminal viewport rendering for the Closeup pane and director drawer.

use crate::presentation::theme::Style;
use crate::presentation::widgets;
use crate::usecase::application::terminal_selection::TerminalPoint;

/// Presentation-only material for the selected live terminal.
///
/// Runtime code polls daemon-owned rows and supplies this value each frame. It
/// never enters controller reducer state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalViewProjection {
    /// Rendered screen rows beginning at `row_offset`. A caller may provide only
    /// the visible window when no retained selection needs the complete history.
    pub rows: Vec<String>,
    /// Retained-terminal row represented by `rows[0]`.
    pub row_offset: usize,
    /// Total retained rows before projection windowing.
    pub total_rows: usize,
    /// Retained rows left below the viewport; zero follows live output.
    pub scroll: usize,
    /// Presentation-safe terminal feedback. This owns the component footer.
    pub feedback: Option<String>,
}

/// Rows above the right-pane live-terminal component.
pub const RIGHT_PANE_CONTENT_TOP: usize = 3;
/// Blank breathing row plus footer reserved below live-terminal content.
pub const FOOTER_ROWS: usize = 2;

/// Exact frame-cell rectangle occupied by retained terminal cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewportGeometry {
    pub left: usize,
    pub top: usize,
    pub rows: usize,
    pub cols: usize,
}

/// First retained terminal row in a bottom-anchored viewport.
#[must_use]
pub fn window_start(total_rows: usize, content_rows: usize, scroll: usize) -> usize {
    total_rows.saturating_sub(content_rows.saturating_add(scroll))
}

/// Map a frame-cell pointer into the retained row/column rendered by a
/// bottom-anchored live-terminal viewport.
#[must_use]
pub fn retained_point_at(
    viewport: ViewportGeometry,
    total_rows: usize,
    scroll: usize,
    column: u16,
    row: u16,
) -> Option<TerminalPoint> {
    let column = usize::from(column).checked_sub(viewport.left)?;
    let content_row = usize::from(row).checked_sub(viewport.top)?;
    if column >= viewport.cols || content_row >= viewport.rows {
        return None;
    }
    Some(TerminalPoint {
        row: window_start(total_rows, viewport.rows, scroll) + content_row,
        column,
    })
}

/// Retained live-terminal rows clipped into a bottom-anchored content window.
#[must_use]
pub fn viewport_rows(
    view: &TerminalViewProjection,
    width: usize,
    content_rows: usize,
) -> Vec<String> {
    let retained_start = window_start(view.total_rows, content_rows, view.scroll);
    let start = retained_start.saturating_sub(view.row_offset);
    view.rows
        .iter()
        .skip(start)
        .take(content_rows)
        .map(|line| widgets::clip_to_width(line, width))
        .collect()
}

/// Render a live-terminal component into an exact-height region.
///
/// Terminal feedback owns the footer when present; otherwise `footer_hint` is
/// retained. Rows between `content_rows` and the footer are caller-owned chrome,
/// such as the Closeup pane's blank breathing row.
#[must_use]
pub fn render(
    view: &TerminalViewProjection,
    width: usize,
    height: usize,
    content_rows: usize,
    footer_hint: &str,
) -> Vec<String> {
    if height == 0 {
        return Vec::new();
    }
    let footer = view.feedback.as_deref().unwrap_or(footer_hint);
    let footer = Style::new()
        .dim()
        .paint(&widgets::clip_to_width(footer, width));
    if height == 1 {
        return vec![footer];
    }
    let content_rows = content_rows.min(height.saturating_sub(1));
    let mut rows = viewport_rows(view, width, content_rows);
    rows.resize(content_rows, String::new());
    rows.resize(height.saturating_sub(1), String::new());
    rows.push(footer);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_is_bottom_anchored_and_applies_scroll_and_row_offset() {
        let retained = (0..10).map(|row| format!("row {row}")).collect::<Vec<_>>();
        let all_rows = TerminalViewProjection {
            rows: retained.clone(),
            row_offset: 0,
            total_rows: retained.len(),
            scroll: 0,
            feedback: None,
        };
        assert_eq!(
            viewport_rows(&all_rows, 80, 3),
            vec!["row 7", "row 8", "row 9"]
        );

        let scrolled = TerminalViewProjection {
            scroll: 2,
            ..all_rows.clone()
        };
        assert_eq!(
            viewport_rows(&scrolled, 80, 3),
            vec!["row 5", "row 6", "row 7"]
        );

        let projected_window = TerminalViewProjection {
            rows: retained[7..].to_vec(),
            row_offset: 7,
            ..all_rows
        };
        assert_eq!(
            viewport_rows(&projected_window, 80, 3),
            vec!["row 7", "row 8", "row 9"]
        );
    }

    #[test]
    fn retained_pointer_mapping_shares_bounds_and_bottom_anchor_policy() {
        let viewport = ViewportGeometry {
            left: 10,
            top: 4,
            rows: 3,
            cols: 5,
        };
        assert_eq!(
            retained_point_at(viewport, 20, 2, 12, 5),
            Some(TerminalPoint { row: 16, column: 2 })
        );
        for (column, row) in [(9, 5), (15, 5), (12, 3), (12, 7)] {
            assert_eq!(retained_point_at(viewport, 20, 2, column, row), None);
        }
        assert_eq!(
            retained_point_at(viewport, 2, usize::MAX, 10, 4),
            Some(TerminalPoint { row: 0, column: 0 })
        );
    }

    #[test]
    fn feedback_replaces_footer_hint_without_entering_content() {
        let view = TerminalViewProjection {
            rows: vec!["output".to_owned()],
            row_offset: 0,
            total_rows: 1,
            scroll: 0,
            feedback: Some("copied 1 line".to_owned()),
        };
        let rows = render(&view, 80, 4, 2, "keys");
        assert_eq!(widgets::strip_ansi(&rows[0]), "output");
        assert_eq!(widgets::strip_ansi(&rows[1]), "");
        assert_eq!(widgets::strip_ansi(&rows[2]), "");
        assert_eq!(widgets::strip_ansi(&rows[3]), "copied 1 line");
    }

    #[test]
    fn render_handles_empty_and_footer_only_regions() {
        let view = TerminalViewProjection::default();
        assert!(render(&view, 80, 0, 0, "keys").is_empty());
        let footer_only = render(&view, 80, 1, 8, "keys");
        assert_eq!(footer_only.len(), 1);
        assert_eq!(widgets::strip_ansi(&footer_only[0]), "keys");
    }
}

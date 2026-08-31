//! Workspace-root terminal drawer anchored to the bottom of Home.
//!
//! The drawer is a workspace-global surface, separate from both managed-session
//! Closeup and the Agent-only Director drawer. This module owns presentation and
//! geometry only; launch, restore, attachment, and input stay in the controller
//! runtime and terminal shell.

use crate::presentation::theme::{Role, Style};
use crate::presentation::views::workspace::TerminalViewProjection;
use crate::presentation::widgets::{self, modal};
use crate::usecase::application::terminal_selection::TerminalPoint;

/// Header glyph for the workspace-root shell surface.
pub const ROOT_TERMINAL_ICON: char = '⌂';
/// Minimum drawer height when enough Home background can remain visible.
pub const MIN_DRAWER_HEIGHT: usize = 10;
/// Maximum drawer height on tall terminals.
pub const MAX_DRAWER_HEIGHT: usize = 32;
/// Background rows retained above a non-full-height drawer, including Home's
/// header. Below this threshold the drawer fills everything below the header.
const MIN_BACKGROUND_HEIGHT: usize = 6;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootTerminalDrawerProjection {
    pub terminal_view: Option<TerminalViewProjection>,
    pub tabs: Vec<RootTerminalTabProjection>,
    pub pending: bool,
    pub feedback: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootTerminalTabProjection {
    pub label: String,
    pub selected: bool,
    pub pending: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootTerminalDrawerGeometry {
    pub left: usize,
    pub top: usize,
    pub width: usize,
    pub height: usize,
    pub full_height: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootTerminalViewport {
    pub rows: usize,
    pub cols: usize,
}

/// Compute a full-width drawer that rises from the bottom edge.
#[must_use]
pub fn geometry(raw_height: usize, raw_width: usize) -> RootTerminalDrawerGeometry {
    geometry_for(raw_height, raw_width, usize::MAX)
}

/// Compute the bottom drawer inside a bounded horizontal band. Director uses
/// its left background band here so both drawers remain fully visible.
#[must_use]
pub fn geometry_for(
    raw_height: usize,
    raw_width: usize,
    available_width: usize,
) -> RootTerminalDrawerGeometry {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let width = width.min(available_width);
    let available = height.saturating_sub(1);
    let desired = height.saturating_mul(11) / 20;
    let coexist_height = desired
        .clamp(MIN_DRAWER_HEIGHT, MAX_DRAWER_HEIGHT)
        .min(available);
    let full_height = height.saturating_sub(coexist_height) < MIN_BACKGROUND_HEIGHT;
    let drawer_height = if full_height {
        available
    } else {
        coexist_height
    };
    RootTerminalDrawerGeometry {
        left: 0,
        top: height.saturating_sub(drawer_height),
        width,
        height: drawer_height,
        full_height,
    }
}

/// Terminal rows/columns inside the drawer's border, padding, and footer.
#[must_use]
pub fn terminal_viewport(raw_height: usize, raw_width: usize) -> RootTerminalViewport {
    terminal_viewport_for(raw_height, raw_width, usize::MAX)
}

#[must_use]
pub fn terminal_viewport_for(
    raw_height: usize,
    raw_width: usize,
    available_width: usize,
) -> RootTerminalViewport {
    let drawer = geometry_for(raw_height, raw_width, available_width);
    RootTerminalViewport {
        // modal::boxed: borders + two padding rows; body: tab strip + footer.
        rows: drawer.height.saturating_sub(6),
        cols: drawer.width.saturating_sub(4),
    }
}

#[must_use]
pub fn terminal_point_at(
    raw_height: usize,
    raw_width: usize,
    rows_len: usize,
    scroll: usize,
    column: u16,
    row: u16,
) -> Option<TerminalPoint> {
    terminal_point_at_for(
        raw_height,
        raw_width,
        usize::MAX,
        rows_len,
        scroll,
        column,
        row,
    )
}

#[must_use]
pub fn terminal_point_at_for(
    raw_height: usize,
    raw_width: usize,
    available_width: usize,
    rows_len: usize,
    scroll: usize,
    column: u16,
    row: u16,
) -> Option<TerminalPoint> {
    let drawer = geometry_for(raw_height, raw_width, available_width);
    let viewport = terminal_viewport_for(raw_height, raw_width, available_width);
    let column = usize::from(column).checked_sub(2)?;
    let content_row = usize::from(row).checked_sub(drawer.top.saturating_add(3))?;
    if column >= viewport.cols || content_row >= viewport.rows {
        return None;
    }
    let start = widgets::live_terminal::window_start(rows_len, viewport.rows, scroll);
    Some(TerminalPoint {
        row: start + content_row,
        column,
    })
}

/// Resolve a click on the visible terminal-only tab strip.
#[must_use]
pub fn tab_at(
    raw_height: usize,
    raw_width: usize,
    tabs: &[RootTerminalTabProjection],
    column: u16,
    row: u16,
) -> Option<usize> {
    let drawer = geometry(raw_height, raw_width);
    if usize::from(row) != drawer.top.saturating_add(2) {
        return None;
    }
    let mut column = usize::from(column).checked_sub(2)?;
    for (index, tab) in tabs.iter().enumerate() {
        let width = widgets::display_width(&format!(
            " {}{} ",
            tab.label,
            if tab.pending { "…" } else { "" }
        ));
        if column < width {
            return Some(index);
        }
        column = column.checked_sub(width.saturating_add(1))?;
    }
    None
}

/// Render the terminal drawer above a dimmed Home background.
#[must_use]
pub fn render_over(
    raw_height: usize,
    raw_width: usize,
    base: &[String],
    projection: &RootTerminalDrawerProjection,
) -> Vec<String> {
    render_over_for(raw_height, raw_width, usize::MAX, base, projection)
}

#[must_use]
pub fn render_over_for(
    raw_height: usize,
    raw_width: usize,
    available_width: usize,
    base: &[String],
    projection: &RootTerminalDrawerProjection,
) -> Vec<String> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let drawer = geometry_for(raw_height, raw_width, available_width);
    let mut frame = (0..height)
        .map(|row| {
            let line = modal::columns(base.get(row).map_or("", String::as_str), 0, width);
            if row == 0 || drawer.width == 0 {
                line
            } else {
                let left = modal::columns(&line, 0, drawer.width);
                let right = modal::columns(&line, drawer.width, width - drawer.width);
                format!("{}{right}", widgets::dim_ansi(&left))
            }
        })
        .collect::<Vec<_>>();
    if drawer.width < 4 || drawer.height == 0 {
        return frame;
    }

    let inner_width = drawer.width.saturating_sub(4);
    let body_height = drawer.height.saturating_sub(4);
    let footer = projection
        .feedback
        .as_deref()
        .unwrap_or("Ctrl-O Ctrl-T: close  ·  Ctrl-O u/d/b: scroll  ·  Ctrl-O x: close terminal");
    let terminal_height = body_height.saturating_sub(1);
    let tab_strip = render_tab_strip(&projection.tabs, inner_width);
    let body = if let Some(view) = &projection.terminal_view {
        let mut body = vec![tab_strip];
        body.extend(widgets::live_terminal::render(
            view,
            inner_width,
            terminal_height,
            terminal_height,
            footer,
        ));
        body
    } else {
        let mut rows = vec![String::new(); body_height];
        if !rows.is_empty() {
            rows[0] = tab_strip;
        }
        if body_height > 1 {
            let message = if projection.pending {
                "Opening workspace terminal…"
            } else {
                "Workspace terminal is unavailable"
            };
            rows[(body_height / 2).max(1)] = Role::Accent.style().bold().paint(message);
            rows[body_height - 1] = Style::new().dim().paint(footer);
        }
        rows
    };
    let title = Role::Accent
        .style()
        .bold()
        .paint(&format!("{ROOT_TERMINAL_ICON} Workspace Terminal"));
    let panel = modal::boxed(&title, inner_width, &body);
    for (offset, panel_line) in panel.iter().take(drawer.height).enumerate() {
        let row = drawer.top + offset;
        if row < frame.len() {
            let suffix = modal::columns(
                &frame[row],
                drawer.width,
                width.saturating_sub(drawer.width),
            );
            frame[row] = format!("{panel_line}\u{1b}[0m{suffix}");
        }
    }
    frame
}

fn render_tab_strip(tabs: &[RootTerminalTabProjection], width: usize) -> String {
    let mut line = String::new();
    for tab in tabs {
        let label = format!(" {}{} ", tab.label, if tab.pending { "…" } else { "" });
        let style = if tab.selected {
            Role::Accent.style().bold().reverse()
        } else {
            Style::new().dim()
        };
        line.push_str(&style.paint(&label));
        line.push(' ');
    }
    line.push_str(&Style::new().dim().paint("Ctrl-O n: new"));
    modal::columns(&line, 0, width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::widgets::{display_width, strip_ansi};

    #[test]
    fn geometry_rises_from_bottom_and_falls_back_below_header() {
        let normal = geometry(30, 100);
        assert_eq!(normal.height, 16);
        assert_eq!(normal.top + normal.height, 30);
        assert!(!normal.full_height);

        let short = geometry(12, 80);
        assert_eq!(short.top, 1);
        assert_eq!(short.height, 11);
        assert!(short.full_height);
        assert_eq!(geometry(0, 0), geometry(24, 80));

        let bounded = geometry_for(30, 100, 40);
        assert_eq!(bounded.width, 40);
        assert_eq!(bounded.left, 0);
    }

    #[test]
    fn viewport_and_pointer_follow_bottom_drawer_content() {
        assert_eq!(
            terminal_viewport(30, 100),
            RootTerminalViewport { rows: 10, cols: 96 }
        );
        let drawer = geometry(30, 100);
        assert_eq!(
            terminal_point_at(
                30,
                100,
                20,
                0,
                2,
                u16::try_from(drawer.top + 3).expect("test geometry fits u16"),
            ),
            Some(TerminalPoint { row: 10, column: 0 })
        );
        assert_eq!(terminal_point_at(30, 100, 20, 0, 1, 0), None);
        assert_eq!(terminal_point_at(30, 100, 20, 0, 98, 29), None);
        assert_eq!(terminal_viewport_for(30, 100, 40).cols, 36);
        assert_eq!(terminal_point_at_for(30, 100, 40, 20, 0, 41, 29), None);
    }

    #[test]
    fn tab_hit_test_matches_the_rendered_terminal_strip() {
        let tabs = vec![
            RootTerminalTabProjection {
                label: "Terminal 1".to_owned(),
                selected: true,
                pending: false,
            },
            RootTerminalTabProjection {
                label: "Terminal 2".to_owned(),
                selected: false,
                pending: true,
            },
        ];
        let top = geometry(30, 100).top;
        let tab_row = u16::try_from(top + 2).unwrap();
        let body_row = u16::try_from(top + 3).unwrap();
        assert_eq!(tab_at(30, 100, &tabs, 2, tab_row), Some(0));
        assert_eq!(tab_at(30, 100, &tabs, 15, tab_row), Some(1));
        assert_eq!(tab_at(30, 100, &tabs, 29, tab_row), None);
        assert_eq!(tab_at(30, 100, &tabs, 2, body_row), None);
    }

    #[test]
    fn render_dims_background_and_places_terminal_at_bottom() {
        let base = (0..30)
            .map(|row| format!("background {row}"))
            .collect::<Vec<_>>();
        let projection = RootTerminalDrawerProjection {
            terminal_view: Some(TerminalViewProjection {
                rows: vec!["root output".to_owned()],
                row_offset: 0,
                total_rows: 1,
                scroll: 0,
                feedback: None,
            }),
            tabs: vec![
                RootTerminalTabProjection {
                    label: "Terminal 1".to_owned(),
                    selected: true,
                    pending: false,
                },
                RootTerminalTabProjection {
                    label: "Terminal 2".to_owned(),
                    selected: false,
                    pending: false,
                },
            ],
            pending: false,
            feedback: None,
        };
        let frame = render_over(30, 100, &base, &projection);
        assert_eq!(frame.len(), 30);
        assert!(strip_ansi(&frame[0]).contains("background 0"));
        assert!(strip_ansi(&frame[geometry(30, 100).top]).contains("Workspace Terminal"));
        assert!(
            frame
                .iter()
                .any(|line| strip_ansi(line).contains("root output"))
        );
        assert!(frame.iter().all(|line| display_width(line) == 100));

        let bounded_base = (0..30)
            .map(|row| format!("{}right background {row}", " ".repeat(50)))
            .collect::<Vec<_>>();
        let bounded = render_over_for(30, 100, 40, &bounded_base, &projection);
        assert!(strip_ansi(&bounded[geometry_for(30, 100, 40).top]).contains("Workspace Terminal"));
        assert!(strip_ansi(&bounded[29]).contains("right background 29"));
    }

    #[test]
    fn render_handles_compact_pending_and_unavailable_states() {
        let compact = render_over(
            1,
            3,
            &["abc".to_owned()],
            &RootTerminalDrawerProjection::default(),
        );
        assert_eq!(compact, vec!["abc"]);
        assert_eq!(
            render_over(4, 80, &[], &RootTerminalDrawerProjection::default()).len(),
            4
        );

        for (pending, message) in [
            (true, "Opening workspace terminal…"),
            (false, "Workspace terminal is unavailable"),
        ] {
            let frame = render_over(
                12,
                80,
                &[],
                &RootTerminalDrawerProjection {
                    terminal_view: None,
                    tabs: Vec::new(),
                    pending,
                    feedback: Some("terminal feedback".to_owned()),
                },
            );
            assert!(frame.iter().any(|line| strip_ansi(line).contains(message)));
            assert!(
                frame
                    .iter()
                    .any(|line| strip_ansi(line).contains("terminal feedback"))
            );
        }
    }
}

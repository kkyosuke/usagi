//! Workspace Agent drawer shell.
//!
//! This view owns only presentation and geometry. It does not inventory,
//! launch, resume, attach, or forward input to an Agent runtime. The projection
//! deliberately already has seams for conversation choices and terminal rows so
//! later runtime work can populate the shell without changing its layout or
//! input ownership.

use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::{self, modal};

/// Desired lower bound while the drawer can coexist with a visible background.
pub const MIN_DRAWER_WIDTH: usize = 56;
/// Maximum drawer width on wide terminals.
pub const MAX_DRAWER_WIDTH: usize = 96;
/// Minimum background strip kept visible beside a non-full-width drawer.
const MIN_BACKGROUND_WIDTH: usize = 24;

/// One presentation-safe conversation choice.
///
/// Inventory identity remains outside the view. A later controller/runtime may
/// associate this display value with its own stable key and feed the selected
/// projection into this shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceAgentConversation {
    pub label: String,
    pub selected: bool,
}

/// Pure material accepted by the drawer renderer.
///
/// The empty default is the only material produced in this issue: no Agent is
/// automatically launched or resumed, and `New` remains a disabled affordance.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceAgentDrawerProjection {
    pub conversations: Vec<WorkspaceAgentConversation>,
    pub terminal_rows: Vec<String>,
}

/// Right-anchored drawer rectangle in terminal cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceAgentDrawerGeometry {
    pub left: usize,
    pub top: usize,
    pub width: usize,
    pub height: usize,
    pub full_width: bool,
}

/// Future Agent terminal viewport inside the drawer, independent from the
/// managed-session Closeup pane's viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceAgentTerminalViewport {
    pub rows: usize,
    pub cols: usize,
}

/// Compute the drawer rectangle from terminal geometry.
///
/// The normal width is 60%, clamped to 56…96 columns. If keeping that minimum
/// would leave less than 24 columns of background, the drawer becomes full
/// width. A zero terminal dimension follows the TUI-wide 80×24 normalization.
#[must_use]
pub fn geometry(raw_height: usize, raw_width: usize) -> WorkspaceAgentDrawerGeometry {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let desired = width.saturating_mul(3) / 5;
    let coexist_width = desired.clamp(MIN_DRAWER_WIDTH, MAX_DRAWER_WIDTH).min(width);
    let full_width = width.saturating_sub(coexist_width) < MIN_BACKGROUND_WIDTH;
    let drawer_width = if full_width { width } else { coexist_width };
    WorkspaceAgentDrawerGeometry {
        left: width.saturating_sub(drawer_width),
        // Home's top header remains visible and owns the drawer toggle button.
        top: 1.min(height),
        width: drawer_width,
        height: height.saturating_sub(1),
        full_width,
    }
}

/// Compute the terminal viewport reserved inside the drawer.
///
/// This intentionally does not call `workspace::terminal_viewport`: the drawer
/// has its own border, selector, breathing row, and footer chrome. Runtime work
/// can therefore resize a Workspace Agent terminal without confusing it with
/// the managed-session Closeup terminal.
#[must_use]
pub fn terminal_viewport(raw_height: usize, raw_width: usize) -> WorkspaceAgentTerminalViewport {
    let drawer = geometry(raw_height, raw_width);
    WorkspaceAgentTerminalViewport {
        // top/bottom borders + selector + separator/breathing row + footer
        rows: drawer.height.saturating_sub(5),
        // left/right borders and one cell of padding on both sides
        cols: drawer.width.saturating_sub(4),
    }
}

/// Render the drawer over a dimmed Home frame.
#[must_use]
pub fn render_over(
    raw_height: usize,
    raw_width: usize,
    base: &[String],
    projection: &WorkspaceAgentDrawerProjection,
) -> Vec<String> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let drawer = geometry(raw_height, raw_width);
    let mut frame = (0..height)
        .map(|row| {
            let line = modal::columns(base.get(row).map_or("", String::as_str), 0, width);
            if row == 0 {
                line
            } else {
                widgets::dim_ansi(&line)
            }
        })
        .collect::<Vec<_>>();

    if drawer.width < 4 || drawer.height == 0 {
        return frame;
    }

    let inner_width = drawer.width.saturating_sub(4);
    let body_height = drawer.height.saturating_sub(2);
    let body = drawer_body(inner_width, body_height, projection);
    let title = Role::Accent.style().bold().paint("Workspace Agent");
    let panel = modal::boxed(&title, inner_width, &body);

    // The panel is `drawer.height` rows and is anchored at `drawer.top`, so it
    // always fits inside the `frame.len()` == height rows built above. Bound the
    // splice by the remaining band so the row index can never leave the frame.
    let band = frame.len().saturating_sub(drawer.top);
    for (offset, panel_line) in panel.iter().take(band).enumerate() {
        let row = drawer.top + offset;
        let background = &frame[row];
        let prefix = modal::columns(background, 0, drawer.left);
        frame[row] = format!("{prefix}{panel_line}\u{1b}[0m");
    }
    frame
}

fn drawer_body(
    width: usize,
    height: usize,
    projection: &WorkspaceAgentDrawerProjection,
) -> Vec<String> {
    if height == 0 {
        return Vec::new();
    }
    let mut rows = vec![selector_row(width, projection)];
    if height > 1 {
        rows.push(Style::new().dim().paint(&"─".repeat(width)));
    }

    let footer = Style::new()
        .dim()
        .paint("Esc / Ctrl-O g: close  ·  New unavailable");
    let content_capacity = height.saturating_sub(rows.len() + 1);
    if projection.terminal_rows.is_empty() {
        let before = content_capacity.saturating_sub(3) / 2;
        rows.extend(std::iter::repeat_n(String::new(), before));
        if content_capacity > before {
            rows.push(Role::Accent.style().bold().paint("No conversations yet"));
        }
        if content_capacity > before + 1 {
            rows.push(
                Style::new()
                    .dim()
                    .paint("Workspace Agent inventory is not connected."),
            );
        }
        if content_capacity > before + 2 {
            rows.push(
                Style::new()
                    .dim()
                    .paint("Choose New after the launcher is enabled."),
            );
        }
    } else {
        rows.extend(
            projection
                .terminal_rows
                .iter()
                .take(content_capacity)
                .cloned(),
        );
    }
    rows.truncate(height.saturating_sub(1));
    rows.resize(height.saturating_sub(1), String::new());
    rows.push(footer);
    rows.into_iter()
        .map(|row| widgets::clip_to_width(&row, width))
        .collect()
}

fn selector_row(width: usize, projection: &WorkspaceAgentDrawerProjection) -> String {
    let selected = projection
        .conversations
        .iter()
        .find(|conversation| conversation.selected)
        .or_else(|| projection.conversations.first())
        .map_or("No conversations", |conversation| {
            conversation.label.as_str()
        });
    let new = Style::new().dim().paint("[ New ]");
    let prefix = format!("Conversation  [{selected}]");
    let reserved = widgets::display_width(&new).saturating_add(2);
    let prefix = widgets::clip_to_width(&prefix, width.saturating_sub(reserved));
    let gap = width
        .saturating_sub(widgets::display_width(&prefix))
        .saturating_sub(widgets::display_width(&new));
    format!("{prefix}{}{new}", " ".repeat(gap))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::widgets::{display_width, strip_ansi};

    #[test]
    fn geometry_clamps_normal_boundary_and_wide_sizes() {
        assert_eq!(
            geometry(24, 100),
            WorkspaceAgentDrawerGeometry {
                left: 40,
                top: 1,
                width: 60,
                height: 23,
                full_width: false,
            }
        );
        assert_eq!(geometry(24, 80).width, MIN_DRAWER_WIDTH);
        assert!(!geometry(24, 80).full_width);
        assert_eq!(geometry(24, 200).width, MAX_DRAWER_WIDTH);
    }

    #[test]
    fn narrow_and_zero_geometry_use_safe_full_width_fallbacks() {
        let narrow = geometry(5, 79);
        assert_eq!(narrow.left, 0);
        assert_eq!(narrow.width, 79);
        assert!(narrow.full_width);

        let zero = geometry(0, 0);
        assert_eq!(zero, geometry(24, 80));
        assert_eq!(
            terminal_viewport(0, 0),
            WorkspaceAgentTerminalViewport { rows: 18, cols: 52 }
        );
        assert_eq!(
            terminal_viewport(1, 1),
            WorkspaceAgentTerminalViewport { rows: 0, cols: 0 }
        );
    }

    #[test]
    fn terminal_viewport_is_independent_from_the_closeup_right_pane() {
        assert_eq!(
            terminal_viewport(24, 100),
            WorkspaceAgentTerminalViewport { rows: 18, cols: 56 }
        );
        assert_ne!(
            (
                terminal_viewport(24, 100).rows,
                terminal_viewport(24, 100).cols
            ),
            crate::presentation::views::workspace::terminal_viewport(24, 100)
        );
    }

    #[test]
    fn empty_drawer_dims_background_and_renders_disabled_shell() {
        let base = (0..24)
            .map(|row| format!("background {row}"))
            .collect::<Vec<_>>();
        let frame = render_over(24, 100, &base, &WorkspaceAgentDrawerProjection::default());
        assert_eq!(frame.len(), 24);
        assert!(frame.iter().all(|line| display_width(line) == 100));
        let text = frame
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Workspace Agent"));
        assert!(text.contains("Conversation  [No conversations]"));
        assert!(text.contains("[ New ]"));
        assert!(text.contains("No conversations yet"));
        assert!(text.contains("New unavailable"));
        assert!(frame[1].contains("\u{1b}[2m"));
        assert!(!frame[0].contains("\u{1b}[2m"));
    }

    #[test]
    fn populated_projection_renders_selected_conversation_and_terminal_rows() {
        let projection = WorkspaceAgentDrawerProjection {
            conversations: vec![
                WorkspaceAgentConversation {
                    label: "older".to_owned(),
                    selected: false,
                },
                WorkspaceAgentConversation {
                    label: "active conversation".to_owned(),
                    selected: true,
                },
            ],
            terminal_rows: vec![
                "agent output one".to_owned(),
                "agent output two".to_owned(),
                "agent output three".to_owned(),
            ],
        };
        let frame = render_over(12, 80, &vec![String::new(); 12], &projection);
        let text = frame
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Conversation  [active conversation]"));
        assert!(text.contains("agent output one"));
        assert!(text.contains("agent output two"));
        assert!(text.contains("agent output three"));
        assert!(!text.contains("No conversations yet"));
    }

    #[test]
    fn renderer_handles_tiny_resize_and_cjk_choice_without_style_leak() {
        let projection = WorkspaceAgentDrawerProjection {
            conversations: vec![WorkspaceAgentConversation {
                label: "会話の履歴".to_owned(),
                selected: true,
            }],
            terminal_rows: Vec::new(),
        };
        for (height, width) in [(0, 0), (1, 1), (3, 8), (12, 56), (24, 200)] {
            let frame = render_over(height, width, &[], &projection);
            let (height, width) = widgets::normalize_size(height, width);
            assert_eq!(frame.len(), height);
            assert!(frame.iter().all(|line| display_width(line) == width));
            assert!(
                frame
                    .iter()
                    .all(|line| line.ends_with("\u{1b}[0m") || !line.contains('\u{1b}'))
            );
        }
    }
}

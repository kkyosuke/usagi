//! Director mode drawer shell.
//!
//! This view owns only presentation and geometry. It does not inventory,
//! launch, resume, attach, or forward input to an Agent runtime. The controller
//! supplies the installed CLI picker projection and the runtime supplies
//! conversation/terminal rows.

use crate::presentation::theme::{Role, Style};
use crate::presentation::views::workspace::TerminalViewProjection;
use crate::presentation::widgets::{self, modal};
use crate::usecase::application::controller::DirectorRoute;
use crate::usecase::application::terminal_selection::TerminalPoint;

/// Desired lower bound while the drawer can coexist with a visible background.
pub const MIN_DRAWER_WIDTH: usize = 56;
/// Maximum drawer width on wide terminals.
pub const MAX_DRAWER_WIDTH: usize = 96;
/// Minimum background strip kept visible beside a non-full-width drawer.
const MIN_BACKGROUND_WIDTH: usize = 24;
/// Standard Unicode Director marker; it requires no private-use patched font.
pub const DIRECTOR_ICON: char = '♛';
/// Rows of drawer chrome the launch picker's candidate rows never get: the Home
/// header row above the drawer, the panel's two borders and two vertical padding
/// rows, the route breadcrumb, its separator, and the footer hint.
const PICKER_CHROME_ROWS: usize = 8;
const _: () = assert!(
    PICKER_CHROME_ROWS == crate::usecase::application::controller::DIRECTOR_PICKER_CHROME_ROWS
);
/// Footer shown while the picker has room for the highlighted candidate.
const PICKER_HINT: &str = "↑↓: select  ·  Enter: launch  ·  Esc: cancel";
/// Footer shown when the drawer cannot draw a single candidate row. The reducer
/// gates Enter on the same capacity, so this states the only way forward.
const PICKER_TOO_SHORT_HINT: &str = "Terminal too short to choose  ·  Esc: cancel";

/// One presentation-safe conversation choice.
///
/// Inventory identity remains outside the view. A later controller/runtime may
/// associate this display value with its own stable key and feed the selected
/// projection into this shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectorConversation {
    pub label: String,
    pub selected: bool,
}

/// One safe row in the Director's organization overview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectorOrganizationRow {
    pub depth: usize,
    pub label: String,
    pub status: String,
}

/// Presentation-safe state of the drawer's explicit `New` chooser.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum DirectorNewProjection {
    /// The chooser is closed and New may be opened.
    #[default]
    Ready,
    /// Installed CLI labels in deterministic order, with one highlighted row.
    Choosing {
        candidates: Vec<String>,
        selected: usize,
    },
    /// No supported Agent CLI is installed.
    Empty,
    /// One confirmed root launch is fenced until its matching completion.
    Launching,
}

/// Pure material accepted by the drawer renderer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirectorDrawerProjection {
    /// Whether this drawer currently owns workspace input.
    pub focused: bool,
    /// Explicit screen inside the persistent Director shell.
    pub route: DirectorRoute,
    pub conversations: Vec<DirectorConversation>,
    pub organization: Vec<DirectorOrganizationRow>,
    pub terminal_view: Option<TerminalViewProjection>,
    /// Safe reason for a selected interrupted conversation, outside PTY output.
    pub interrupted_detail: Option<String>,
    /// Drawer feedback used when the selected conversation has no live terminal.
    pub feedback: Option<String>,
    pub new: DirectorNewProjection,
}

/// Right-anchored drawer rectangle in terminal cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectorDrawerGeometry {
    pub left: usize,
    pub top: usize,
    pub width: usize,
    pub height: usize,
    pub full_width: bool,
}

/// Future Agent terminal viewport inside the drawer, independent from the
/// managed-session Closeup pane's viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectorTerminalViewport {
    pub rows: usize,
    pub cols: usize,
}

/// Compute the drawer rectangle from terminal geometry.
///
/// The normal width is 60%, clamped to 56…96 columns. If keeping that minimum
/// would leave less than 24 columns of background, the drawer becomes full
/// width. A zero terminal dimension follows the TUI-wide 80×24 normalization.
#[must_use]
pub fn geometry(raw_height: usize, raw_width: usize) -> DirectorDrawerGeometry {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let desired = width.saturating_mul(3) / 5;
    let coexist_width = desired.clamp(MIN_DRAWER_WIDTH, MAX_DRAWER_WIDTH).min(width);
    let full_width = width.saturating_sub(coexist_width) < MIN_BACKGROUND_WIDTH;
    let drawer_width = if full_width { width } else { coexist_width };
    DirectorDrawerGeometry {
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
/// can therefore resize a director terminal without confusing it with
/// the managed-session Closeup terminal.
#[must_use]
pub fn terminal_viewport(raw_height: usize, raw_width: usize) -> DirectorTerminalViewport {
    let drawer = geometry(raw_height, raw_width);
    DirectorTerminalViewport {
        // borders + padding + selector + separator + footer
        rows: drawer.height.saturating_sub(7),
        // left/right borders and one cell of padding on both sides
        cols: drawer.width.saturating_sub(4),
    }
}

/// Candidate rows the `New` picker can draw at this terminal size.
///
/// The picker's viewport follows the selection, so a non-zero capacity always
/// shows the highlighted CLI. A zero capacity draws no candidate at all, which
/// is why the reducer refuses to launch from it.
#[must_use]
pub fn picker_capacity(raw_height: usize, raw_width: usize) -> usize {
    let (height, _) = widgets::normalize_size(raw_height, raw_width);
    height.saturating_sub(PICKER_CHROME_ROWS)
}

/// Map a frame-cell pointer into the retained root Agent terminal viewport.
#[must_use]
pub fn terminal_point_at(
    raw_height: usize,
    raw_width: usize,
    rows_len: usize,
    scroll: usize,
    column: u16,
    row: u16,
) -> Option<TerminalPoint> {
    let drawer = geometry(raw_height, raw_width);
    let viewport = terminal_viewport(raw_height, raw_width);
    widgets::live_terminal::retained_point_at(
        widgets::live_terminal::ViewportGeometry {
            left: drawer.left.saturating_add(2),
            top: drawer.top.saturating_add(4),
            rows: viewport.rows,
            cols: viewport.cols,
        },
        rows_len,
        scroll,
        column,
        row,
    )
}

/// Whether a frame-cell press lands on the drawer's right-aligned `New`
/// affordance. The launch-in-progress label is inert.
#[must_use]
pub fn new_button_at(
    raw_height: usize,
    raw_width: usize,
    column: u16,
    row: u16,
    launching: bool,
) -> bool {
    if launching {
        return false;
    }
    let drawer = geometry(raw_height, raw_width);
    if usize::from(row) != drawer.top.saturating_add(2) {
        return false;
    }
    let right = drawer.left.saturating_add(drawer.width).saturating_sub(2);
    let label = "[ New ]";
    let left = right.saturating_sub(widgets::display_width(label));
    (left..right).contains(&usize::from(column))
}

/// Render the drawer over a dimmed Home frame.
#[must_use]
pub fn render_over(
    raw_height: usize,
    raw_width: usize,
    base: &[String],
    projection: &DirectorDrawerProjection,
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
    // `modal::boxed` adds the top/bottom borders and one padding row inside
    // each border. Reserve all four rows so the bottom border stays on-screen.
    let body_height = drawer.height.saturating_sub(4);
    let body = drawer_body(inner_width, body_height, projection);
    let title = if projection.focused {
        Role::Accent
            .style()
            .bold()
            .reverse()
            .paint(&format!("{DIRECTOR_ICON} Director · FOCUS"))
    } else {
        Style::new()
            .dim()
            .paint(&format!("{DIRECTOR_ICON} Director · click to focus"))
    };
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

fn drawer_body(width: usize, height: usize, projection: &DirectorDrawerProjection) -> Vec<String> {
    if height == 0 {
        return Vec::new();
    }
    let mut rows = vec![breadcrumb_row(width, projection)];
    if height > 1 {
        rows.push(Style::new().dim().paint(&"─".repeat(width)));
    }

    if let DirectorNewProjection::Choosing {
        candidates,
        selected,
    } = &projection.new
    {
        return provider_picker_body(width, height, rows, candidates, *selected);
    }
    if matches!(projection.new, DirectorNewProjection::Launching) {
        return launching_body(width, height, rows);
    }
    if matches!(projection.new, DirectorNewProjection::Empty) {
        return empty_provider_body(width, height, rows);
    }

    match projection.route {
        DirectorRoute::Console => {
            let footer = "Ctrl-O b: Organization · Ctrl-O g: close";
            if let Some(view) = &projection.terminal_view {
                return terminal_conversation_body(width, height, rows, view, footer);
            }
            if let Some(detail) = &projection.interrupted_detail {
                rows.push(Style::new().dim().paint(detail));
            }
            rows.truncate(height.saturating_sub(1));
            rows.resize(height.saturating_sub(1), String::new());
            rows.push(
                Style::new()
                    .dim()
                    .paint(projection.feedback.as_deref().unwrap_or(footer)),
            );
            rows.into_iter()
                .map(|row| widgets::clip_to_width(&row, width))
                .collect()
        }
        DirectorRoute::Organization => organization_body(width, height, rows, projection),
    }
}

fn organization_body(
    width: usize,
    height: usize,
    mut rows: Vec<String>,
    projection: &DirectorDrawerProjection,
) -> Vec<String> {
    let footer_hint = "Ctrl-O n New · Enter Console · Esc close";
    let content_capacity = height.saturating_sub(rows.len() + 1);
    if !projection.conversations.is_empty() {
        rows.push(Role::Accent.style().bold().paint("Conversations"));
        for conversation in &projection.conversations {
            let marker = if conversation.selected { "›" } else { " " };
            rows.push(format!("{marker} {}", conversation.label));
        }
        rows.push(String::new());
    }
    let selected_conversation = projection
        .conversations
        .iter()
        .any(|conversation| conversation.selected);
    if selected_conversation && !projection.organization.is_empty() {
        rows.push(Role::Accent.style().bold().paint("Agent / Sessions"));
        for member in &projection.organization {
            let branch = if member.depth == 0 { "" } else { "└─ " };
            rows.push(format!(
                "{}{}{}  {}",
                "  ".repeat(member.depth),
                branch,
                member.label,
                Style::new().dim().paint(&member.status)
            ));
        }
    } else if projection.conversations.is_empty() {
        empty_conversation_rows(&mut rows, content_capacity);
    } else if rows.len() < height.saturating_sub(1) {
        rows.push(
            Style::new()
                .dim()
                .paint("Select a Conversation to inspect its Agent / Session tree."),
        );
    }
    rows.truncate(height.saturating_sub(1));
    rows.resize(height.saturating_sub(1), String::new());
    rows.push(
        Style::new()
            .dim()
            .paint(projection.feedback.as_deref().unwrap_or(footer_hint)),
    );
    rows.into_iter()
        .map(|row| widgets::clip_to_width(&row, width))
        .collect()
}

fn terminal_conversation_body(
    width: usize,
    height: usize,
    mut rows: Vec<String>,
    view: &TerminalViewProjection,
    footer_hint: &str,
) -> Vec<String> {
    let terminal_rows = height.saturating_sub(rows.len() + 1);
    rows.extend(widgets::live_terminal::viewport_rows(
        view,
        width,
        terminal_rows,
    ));
    rows.resize(height.saturating_sub(1), String::new());
    rows.truncate(height.saturating_sub(1));
    rows.resize(height.saturating_sub(1), String::new());
    rows.push(
        Style::new()
            .dim()
            .paint(view.feedback.as_deref().unwrap_or(footer_hint)),
    );
    rows
}

fn launching_body(width: usize, height: usize, mut rows: Vec<String>) -> Vec<String> {
    let content_capacity = height.saturating_sub(rows.len() + 1);
    let before = content_capacity.saturating_sub(2) / 2;
    rows.extend(std::iter::repeat_n(String::new(), before));
    if content_capacity > before {
        rows.push(Role::Accent.style().bold().paint("Starting…"));
    }
    if content_capacity > before + 1 {
        rows.push(Style::new().dim().paint("Waiting for daemon confirmation."));
    }
    rows.truncate(height.saturating_sub(1));
    rows.resize(height.saturating_sub(1), String::new());
    rows.push(
        Style::new()
            .dim()
            .paint("Launch in progress · Ctrl-O g: close"),
    );
    rows.into_iter()
        .map(|row| widgets::clip_to_width(&row, width))
        .collect()
}

fn empty_provider_body(width: usize, height: usize, mut rows: Vec<String>) -> Vec<String> {
    let content_capacity = height.saturating_sub(rows.len() + 1);
    let before = content_capacity.saturating_sub(2) / 2;
    rows.extend(std::iter::repeat_n(String::new(), before));
    if content_capacity > before {
        rows.push(Role::Accent.style().bold().paint("No Agent CLI installed"));
    }
    if content_capacity > before + 1 {
        rows.push(Style::new().dim().paint("Install claude, codex, or agy."));
    }
    rows.truncate(height.saturating_sub(1));
    rows.resize(height.saturating_sub(1), String::new());
    rows.push(Style::new().dim().paint("Esc: back · Ctrl-O g: close"));
    rows.into_iter()
        .map(|row| widgets::clip_to_width(&row, width))
        .collect()
}

fn empty_conversation_rows(rows: &mut Vec<String>, content_capacity: usize) {
    let before = content_capacity.saturating_sub(3) / 2;
    rows.extend(std::iter::repeat_n(String::new(), before));
    if content_capacity > before {
        rows.push(Role::Accent.style().bold().paint("No conversations yet"));
    }
    if content_capacity > before + 1 {
        rows.push(
            Style::new()
                .dim()
                .paint("Conversation inventory is not connected."),
        );
    }
    if content_capacity <= before + 2 {
        return;
    }
    rows.push(
        Style::new()
            .dim()
            .paint("Choose New to start a conversation."),
    );
}

fn provider_picker_body(
    width: usize,
    height: usize,
    mut rows: Vec<String>,
    candidates: &[String],
    selected: usize,
) -> Vec<String> {
    let content_capacity = height.saturating_sub(rows.len() + 1);
    rows.extend(picker_rows(candidates, selected, content_capacity));
    rows.truncate(height.saturating_sub(1));
    rows.resize(height.saturating_sub(1), String::new());
    rows.push(Style::new().dim().paint(if content_capacity == 0 {
        PICKER_TOO_SHORT_HINT
    } else {
        PICKER_HINT
    }));
    rows.into_iter()
        .map(|row| widgets::clip_to_width(&row, width))
        .collect()
}

/// The picker's candidate rows for a `capacity`-row content area.
///
/// The window follows the selection, so the highlighted CLI is on screen
/// whenever there is a content row at all to put it on. A zero capacity draws
/// nothing and the footer says why; the reducer refuses the launch at the same
/// capacity, so `Enter` can never confirm an off-screen candidate.
fn picker_rows(candidates: &[String], selected: usize, capacity: usize) -> Vec<String> {
    let rows = candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            let marker = if index == selected { "›" } else { " " };
            let line = format!("{marker} {candidate}");
            if index == selected {
                Role::Accent.style().bold().paint(&line)
            } else {
                line
            }
        })
        .collect::<Vec<_>>();
    modal::bounded_list_rows(&rows, selected, capacity)
}

fn breadcrumb_row(width: usize, projection: &DirectorDrawerProjection) -> String {
    let new = if matches!(projection.new, DirectorNewProjection::Launching) {
        Style::new().dim().paint("[ Starting… ]")
    } else {
        Role::Accent.style().bold().paint("[ New ]")
    };
    let route = if matches!(projection.new, DirectorNewProjection::Ready) {
        match projection.route {
            DirectorRoute::Organization => "Director / Organization",
            DirectorRoute::Console => "Director / Organization / Console",
        }
    } else if matches!(projection.new, DirectorNewProjection::Launching) {
        "Director / Starting"
    } else {
        "Director / New Conversation"
    };
    let reserved = widgets::display_width(&new).saturating_add(2);
    let prefix = widgets::clip_to_width(route, width.saturating_sub(reserved));
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
            DirectorDrawerGeometry {
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
            DirectorTerminalViewport { rows: 16, cols: 52 }
        );
        assert_eq!(
            terminal_viewport(1, 1),
            DirectorTerminalViewport { rows: 0, cols: 0 }
        );
    }

    #[test]
    fn terminal_viewport_is_independent_from_the_closeup_right_pane() {
        assert_eq!(
            terminal_viewport(24, 100),
            DirectorTerminalViewport { rows: 16, cols: 56 }
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
    fn terminal_pointer_mapping_uses_drawer_content_geometry() {
        let drawer = geometry(24, 100);
        assert_eq!(
            terminal_point_at(
                24,
                100,
                30,
                0,
                u16::try_from(drawer.left + 2).unwrap(),
                u16::try_from(drawer.top + 4).unwrap(),
            ),
            Some(TerminalPoint { row: 14, column: 0 })
        );
        assert_eq!(terminal_point_at(24, 100, 30, 0, 0, 0), None);
        assert_eq!(
            terminal_point_at(
                24,
                100,
                30,
                0,
                u16::try_from(drawer.left + 2).unwrap(),
                u16::try_from(drawer.top + 4 + terminal_viewport(24, 100).rows).unwrap(),
            ),
            None
        );
    }

    #[test]
    fn new_button_hit_test_matches_selector_row_and_is_inert_while_launching() {
        let drawer = geometry(24, 100);
        let row = u16::try_from(drawer.top + 2).unwrap();
        let right = u16::try_from(drawer.left + drawer.width - 3).unwrap();
        assert!(new_button_at(24, 100, right, row, false));
        assert!(!new_button_at(24, 100, right, row, true));
        assert!(!new_button_at(24, 100, 0, row, false));
        assert!(!new_button_at(24, 100, right, row + 1, false));
        assert!(new_button_at(24, 100, right - 6, row, false));
        assert!(!new_button_at(24, 100, right - 7, row, false));
    }

    #[test]
    fn empty_drawer_dims_background_and_renders_new_affordance() {
        let base = (0..24)
            .map(|row| format!("background {row}"))
            .collect::<Vec<_>>();
        let frame = render_over(24, 100, &base, &DirectorDrawerProjection::default());
        assert_eq!(frame.len(), 24);
        assert!(frame.iter().all(|line| display_width(line) == 100));
        let text = frame
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(&format!("{DIRECTOR_ICON} Director")));
        assert!(text.contains("Director / Organization"));
        assert!(text.contains("[ New ]"));
        assert!(text.contains("No conversations yet"));
        assert!(text.contains("Ctrl-O n New"));
        assert!(frame[1].contains("\u{1b}[2m"));
        assert!(!frame[0].contains("\u{1b}[2m"));
        assert!(strip_ansi(&frame[23]).contains('└'));
        assert!(strip_ansi(&frame[23]).ends_with('┘'));
    }

    #[test]
    fn organization_requires_a_selected_conversation_for_its_tree() {
        let projection = DirectorDrawerProjection {
            conversations: vec![DirectorConversation {
                label: "Agent 12345678".into(),
                selected: false,
            }],
            organization: vec![DirectorOrganizationRow {
                depth: 0,
                label: "Director".into(),
                status: "active".into(),
            }],
            ..DirectorDrawerProjection::default()
        };
        let text = drawer_body(72, 10, &projection)
            .into_iter()
            .map(|row| strip_ansi(&row))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("Conversations"));
        assert!(text.contains("Select a Conversation"));
        assert!(!text.contains("Agent / Sessions"));
    }

    #[test]
    fn focus_badge_distinguishes_the_active_director() {
        let projection = DirectorDrawerProjection {
            focused: true,
            conversations: vec![DirectorConversation {
                label: "active".to_owned(),
                selected: true,
            }],
            terminal_view: Some(TerminalViewProjection {
                rows: vec!["agent output".to_owned()],
                row_offset: 0,
                total_rows: 1,
                scroll: 0,
                feedback: None,
            }),
            ..DirectorDrawerProjection::default()
        };
        let frame = render_over(16, 80, &[], &projection);
        let text = frame.join("\n");
        assert!(strip_ansi(&text).contains("Director · FOCUS"));
        assert!(frame.iter().all(|line| display_width(line) == 80));

        let inactive = render_over(
            16,
            80,
            &[],
            &DirectorDrawerProjection {
                focused: false,
                ..projection
            },
        )
        .join("\n");
        assert!(strip_ansi(&inactive).contains("Director · click to focus"));
    }

    #[test]
    fn empty_drawer_omits_detail_when_height_is_too_small() {
        let body = drawer_body(40, 3, &DirectorDrawerProjection::default());
        let text = body
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(body.len(), 3);
        assert!(!text.contains("Choose New to start a conversation."));
    }

    #[test]
    fn organization_projection_renders_depth_and_status() {
        let projection = DirectorDrawerProjection {
            conversations: vec![DirectorConversation {
                label: "Agent 12345678".into(),
                selected: true,
            }],
            organization: vec![
                DirectorOrganizationRow {
                    depth: 0,
                    label: "Director".into(),
                    status: "active".into(),
                },
                DirectorOrganizationRow {
                    depth: 1,
                    label: "triage (manager)".into(),
                    status: "waiting".into(),
                },
                DirectorOrganizationRow {
                    depth: 2,
                    label: "implement (executor)".into(),
                    status: "stopped".into(),
                },
            ],
            ..DirectorDrawerProjection::default()
        };
        let body = drawer_body(52, 10, &projection)
            .into_iter()
            .map(|row| strip_ansi(&row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.contains("Conversations"));
        assert!(body.contains("Agent / Sessions"));
        assert!(body.contains("triage (manager)"));
        assert!(body.contains("implement (executor)"));
        assert!(body.contains("stopped"));
    }

    #[test]
    fn terminal_rows_render_even_when_conversation_inventory_is_empty() {
        let projection = DirectorDrawerProjection {
            route: DirectorRoute::Console,
            terminal_view: Some(TerminalViewProjection {
                rows: vec!["live output without inventory".to_owned()],
                row_offset: 0,
                total_rows: 1,
                scroll: 0,
                feedback: None,
            }),
            ..DirectorDrawerProjection::default()
        };
        let body = drawer_body(52, 9, &projection)
            .into_iter()
            .map(|row| strip_ansi(&row))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(body.contains("live output without inventory"));
        assert!(!body.contains("No conversations yet"));
        assert!(!body.contains("Conversation inventory is not connected."));
    }

    #[test]
    fn interrupted_console_shows_its_detail_and_recovery_feedback() {
        for feedback in [None, Some("Resume was refused".to_owned())] {
            let projection = DirectorDrawerProjection {
                route: DirectorRoute::Console,
                interrupted_detail: Some("This Agent is interrupted".to_owned()),
                feedback: feedback.clone(),
                ..DirectorDrawerProjection::default()
            };
            let frame = render_over(20, 80, &[], &projection);
            let text = frame
                .iter()
                .map(|row| strip_ansi(row))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains("Director / Organization / Console"));
            assert!(text.contains("This Agent is interrupted"));
            assert!(
                text.contains(
                    feedback
                        .as_deref()
                        .unwrap_or("Ctrl-O b: Organization · Ctrl-O g: close")
                )
            );
            assert!(frame.iter().all(|line| display_width(line) == 80));
        }
    }

    #[test]
    fn picker_and_safe_empty_state_render_without_clipping_cjk() {
        let picker = DirectorDrawerProjection {
            new: DirectorNewProjection::Choosing {
                candidates: vec![
                    "claude".to_owned(),
                    "codex".to_owned(),
                    "agy 日本語".to_owned(),
                ],
                selected: 2,
            },
            ..DirectorDrawerProjection::default()
        };
        let frame = render_over(12, 56, &[], &picker);
        let text = frame
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("claude"));
        assert!(text.contains("codex"));
        assert!(text.contains("› agy 日本語"));
        assert!(text.contains("Enter: launch"));
        assert!(frame.iter().all(|line| display_width(line) == 56));

        let empty = DirectorDrawerProjection {
            new: DirectorNewProjection::Empty,
            ..DirectorDrawerProjection::default()
        };
        let text = render_over(12, 56, &[], &empty)
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("No Agent CLI installed"));
        assert!(text.contains("Install claude, codex, or agy"));

        let launching = DirectorDrawerProjection {
            new: DirectorNewProjection::Launching,
            ..DirectorDrawerProjection::default()
        };
        let text = render_over(12, 56, &[], &launching)
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("[ Starting… ]"));
        assert!(text.contains("Director / Starting"));
        assert!(text.contains("Starting…"));
        assert!(text.contains("Waiting for daemon confirmation."));
        assert!(!text.contains("New Conversation"));
        assert!(!text.contains("Start Work Run"));
    }

    fn picker_of(candidates: &[&str], selected: usize) -> DirectorDrawerProjection {
        DirectorDrawerProjection {
            new: DirectorNewProjection::Choosing {
                candidates: candidates.iter().map(|label| (*label).to_owned()).collect(),
                selected,
            },
            ..DirectorDrawerProjection::default()
        }
    }

    #[test]
    fn picker_viewport_follows_the_selection_on_short_terminals() {
        let candidates = ["claude", "codex", "agy"];
        // 10 rows leave two candidate rows, 9 leave one, 8 leave none.
        for height in 8..=10 {
            for selected in 0..candidates.len() {
                let label = format!("height {height}, selected {selected}");
                let frame = render_over(height, 80, &[], &picker_of(&candidates, selected));
                assert!(
                    frame.iter().all(|line| display_width(line) == 80),
                    "{label}"
                );
                let text = frame
                    .iter()
                    .map(|line| strip_ansi(line))
                    .collect::<Vec<_>>();
                let marked = text
                    .iter()
                    .filter(|line| line.contains('›'))
                    .collect::<Vec<_>>();

                if height == 8 {
                    // No content row survives the chrome, so nothing is
                    // highlighted and the footer stops offering Enter — the
                    // reducer refuses the same launch at this height.
                    assert!(marked.is_empty(), "{label}");
                    assert!(
                        text.iter().any(|line| line.contains(PICKER_TOO_SHORT_HINT)),
                        "{label}"
                    );
                    assert!(
                        !text.iter().any(|line| line.contains("Enter: launch")),
                        "{label}"
                    );
                    continue;
                }
                assert_eq!(marked.len(), 1, "{label}");
                assert!(marked[0].contains(candidates[selected]), "{label}");
                assert!(
                    text.iter().any(|line| line.contains("Enter: launch")),
                    "{label}"
                );
            }
        }
    }

    #[test]
    fn picker_capacity_matches_the_rows_the_drawer_draws() {
        let candidates = (0..20).map(|index| format!("cli-{index:02}")).collect();
        let projection = DirectorDrawerProjection {
            new: DirectorNewProjection::Choosing {
                candidates,
                selected: 10,
            },
            ..DirectorDrawerProjection::default()
        };
        for height in 0..=16 {
            let frame = render_over(height, 80, &[], &projection);
            let text = frame
                .iter()
                .map(|line| strip_ansi(line))
                .collect::<Vec<_>>();
            let drawn = text
                .iter()
                .filter(|line| line.contains("cli-") || line.contains(" more"))
                .count();
            assert_eq!(drawn, picker_capacity(height, 80), "height {height}");
            if drawn > 0 {
                assert!(
                    text.iter().any(|line| line.contains("› cli-10")),
                    "height {height}"
                );
            }
        }
    }

    #[test]
    fn picker_rows_keep_the_frame_width_with_wide_and_pre_styled_labels() {
        let candidates = [
            "日本語のエージェント",
            "\u{1b}[1;31mcodex\u{1b}[0m",
            "agy 日本語",
        ];
        for height in 0..=14 {
            for width in [40, 56, 100] {
                for selected in 0..candidates.len() {
                    let frame = render_over(height, width, &[], &picker_of(&candidates, selected));
                    let (height, width) = widgets::normalize_size(height, width);
                    assert_eq!(frame.len(), height);
                    assert!(
                        frame.iter().all(|line| display_width(line) == width),
                        "{height}x{width}, selected {selected}"
                    );
                    assert!(
                        frame
                            .iter()
                            .all(|line| line.ends_with("\u{1b}[0m") || !line.contains('\u{1b}'))
                    );
                }
            }
        }
    }
}

//! Project/session list beside the meadow. It shares the garden's safe state
//! and stable click targets; scrolling never changes the meadow viewport.

use std::ops::Deref;

use usagi_core::domain::id::WorkspaceId;

use super::{GardenFrame, GardenHitbox, GardenSession, agent_status};
use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::{clip_to_width, pad_to_width};

const MIN_PANEL_WIDTH: usize = 34;
const MAX_PANEL_WIDTH: usize = 48;
const CONTENT_TOP: usize = 2;

/// Optional display facts; absent facts are never inferred from a display name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionDetails {
    pub project: Option<(WorkspaceId, String)>,
    pub name: String,
    pub branch: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ViewOptions {
    pub tick: u64,
    pub reduced_motion: bool,
    pub scroll: usize,
}

/// Geometry and bounded scroll position of the list actually drawn this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SidebarViewport {
    pub column: usize,
    pub width: usize,
    pub footer_row: usize,
    pub scroll: usize,
    pub max_scroll: usize,
    pub page_size: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GardenView {
    pub frame: GardenFrame,
    pub sidebar: Option<SidebarViewport>,
}

impl Deref for GardenView {
    type Target = GardenFrame;

    fn deref(&self) -> &Self::Target {
        &self.frame
    }
}

/// Keep at least the compact garden's minimum width next to the list.
#[must_use]
pub fn scene_width(width: usize) -> usize {
    if width < super::MIN_WIDTH + MIN_PANEL_WIDTH + 1 {
        return width;
    }
    let panel = (width / 10 * 3).clamp(MIN_PANEL_WIDTH, MAX_PANEL_WIDTH);
    width - panel - 1
}

/// Compose both surfaces and their hitboxes in a single coordinate space.
#[must_use]
pub fn render(
    height: usize,
    width: usize,
    scope: &str,
    sessions: &[GardenSession],
    options: ViewOptions,
) -> Option<GardenView> {
    let left_width = scene_width(width);
    let mut frame = super::render(
        height,
        left_width,
        scope,
        sessions,
        options.tick,
        options.reduced_motion,
    )?;
    if left_width == width {
        return Some(GardenView {
            frame,
            sidebar: None,
        });
    }
    let column = left_width + 1;
    let panel_width = width - column;
    let content = content_rows(sessions, scope, panel_width);
    let page_size = height - CONTENT_TOP - 1;
    let max_scroll = content.len().saturating_sub(page_size);
    let scroll = options.scroll.min(max_scroll);
    let viewport = SidebarViewport {
        column,
        width: panel_width,
        footer_row: height - 1,
        scroll,
        max_scroll,
        page_size,
    };
    let divider = Style::new().dim().paint("│");
    // One header spans both panes. The list starts beneath it.
    frame.rows[0] = super::header_line(width, scope, sessions);
    frame.rows[1] = format!(
        "{}{divider}{}",
        frame.rows[1],
        pad_to_width(&Style::new().bold().paint(" Sessions"), panel_width),
    );
    for row in CONTENT_TOP..height - 1 {
        let item = content.get(scroll + row - CONTENT_TOP);
        let text = item.map_or("", |item| item.text.as_str());
        frame.rows[row] = format!(
            "{}{divider}{}",
            frame.rows[row],
            pad_to_width(text, panel_width),
        );
        if let Some(target) = item.and_then(|item| item.target) {
            frame.hitboxes.push(GardenHitbox {
                column,
                row,
                width: panel_width,
                height: 1,
                ..target
            });
        }
    }
    let half = panel_width / 2;
    let previous = if scroll > 0 { " ↑ Previous" } else { "" };
    let next = if scroll < max_scroll { "↓ Next " } else { "" };
    frame.rows[height - 1] = format!(
        "{}{divider}{}{}",
        pad_to_width(
            &Style::new()
                .dim()
                .paint(" Garden Action Center · click a usagi · ↑/↓ list · Esc wake"),
            left_width
        ),
        Role::Accent.style().paint(&pad_to_width(previous, half)),
        Role::Accent
            .style()
            .paint(&pad_to_width(next, panel_width - half)),
    );
    Some(GardenView {
        frame,
        sidebar: Some(viewport),
    })
}

struct ListRow {
    text: String,
    target: Option<GardenHitbox>,
}

fn content_rows(sessions: &[GardenSession], scope: &str, width: usize) -> Vec<ListRow> {
    let mut rows = Vec::new();
    // Group by exact workspace identity, retaining deck order even when project
    // labels collide. A standalone renderer uses its injected scope.
    let mut groups: Vec<Vec<&GardenSession>> = Vec::new();
    for session in sessions {
        let identity = session.sidebar.project.as_ref().map(|(id, _)| *id);
        if let Some(group) = groups
            .iter_mut()
            .find(|group| group[0].sidebar.project.as_ref().map(|(id, _)| *id) == identity)
        {
            group.push(session);
        } else {
            groups.push(vec![session]);
        }
    }
    for group in groups {
        let project = group[0]
            .sidebar
            .project
            .as_ref()
            .map_or(scope, |(_, name)| name);
        let count = format!("  {}", group.len());
        rows.push(ListRow {
            text: format!(
                " {} {}{}",
                Role::Warning.style().paint("▱"),
                Style::new().bold().paint(&clip_to_width(
                    project,
                    width.saturating_sub(4 + count.len())
                )),
                Style::new().dim().paint(&count),
            ),
            target: None,
        });
        for session in group {
            session_rows(&mut rows, session, width);
        }
    }
    if sessions.is_empty() {
        rows.push(ListRow {
            text: Style::new().dim().paint(" No sessions"),
            target: None,
        });
    }
    rows
}

fn session_rows(rows: &mut Vec<ListRow>, session: &GardenSession, width: usize) {
    let target = GardenHitbox {
        session_id: session.id,
        agent: None,
        column: 0,
        row: 0,
        width: 0,
        height: 0,
    };
    let agents = agent_status::ordered(&session.agents);
    let name = if session.sidebar.name.is_empty() {
        &session.label
    } else {
        &session.sidebar.name
    };
    let marker = if session.selected { "╭" } else { " " };
    let (style, glyph) = if super::needs_attention(session) {
        (Role::Warning.style(), "◆")
    } else if let Some(agent) = agents.first() {
        let (style, glyph, _, _) = super::dense_agent_appearance(session, *agent);
        (style, glyph)
    } else {
        let (style, _) = super::session_summary(session);
        (style, "○")
    };
    rows.push(ListRow {
        text: format!(
            "{marker} {} {}",
            style.paint(glyph),
            Style::new().bold().paint(name)
        ),
        target: Some(target),
    });
    let edge = if session.selected { "│" } else { " " };
    if !session.sidebar.branch.is_empty() {
        rows.push(ListRow {
            text: format!(
                "{edge}   {}",
                Style::new().dim().paint(&session.sidebar.branch)
            ),
            target: Some(target),
        });
    }
    let summary = if !session.agents_observed {
        "project inactive".to_owned()
    } else if agents.is_empty() {
        super::session_summary(session).1
    } else {
        let noun = if agents.len() == 1 { "agent" } else { "agents" };
        format!("{} {noun}", agents.len())
    };
    rows.push(ListRow {
        text: format!("{edge}   {}", Style::new().dim().paint(&summary)),
        target: Some(target),
    });
    for agent in agents {
        let (style, glyph, status, _) = super::dense_agent_appearance(session, agent);
        let runtime = agent.runtime_id.to_string();
        rows.push(ListRow {
            text: format!(
                "{edge}     {} {}  {}",
                style.paint(glyph),
                style.paint(status),
                Style::new().dim().paint(&runtime[..8]),
            ),
            target: Some(GardenHitbox {
                agent: Some(agent.runtime_id),
                ..target
            }),
        });
    }
    rows.push(ListRow {
        text: if session.selected {
            Style::new()
                .dim()
                .paint(&format!("╰{}", "─".repeat(width.saturating_sub(2))))
        } else {
            String::new()
        },
        target: None,
    });
}

#[cfg(test)]
mod tests;

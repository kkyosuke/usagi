//! Read-only search projection. Execution is revalidated by the resident runtime.

use super::{
    DeckOverlay, Key, OverlayIntent, Path, PathBuf, Role, SessionId, WorkspaceDeck, WorkspaceId,
    WorkspaceSlot, modal,
};
use crate::presentation::widgets::TextInput;
use crate::presentation::workspace_runtime::WorkspaceRuntime;
use crate::usecase::application::controller::Target;
use crate::usecase::application::pane::{PaneKind, TabSelection};
use crate::usecase::{closeup, fuzzy::fuzzy_score, overview};
use usagi_core::domain::presentation_text::sanitize_presentation_line;
use usagi_core::domain::session_lifecycle::SessionLifecycle;

const MAX_QUERY_CHARS: usize = 256;

/// Every choice carries its original identity through filtering and activation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Workspace,
    Session(SessionId),
    Tab {
        target: Target,
        selection: TabSelection,
        kind: PaneKind,
    },
    Command {
        session: Option<SessionId>,
        name: &'static str,
    },
}

impl Action {
    pub(crate) fn session(&self) -> Option<SessionId> {
        match self {
            Self::Session(session) => Some(*session),
            Self::Tab { target, .. } => target.session_id(),
            Self::Command { session, .. } => *session,
            Self::Workspace => None,
        }
    }
}

/// Cached tab membership contains no terminal output or provider-native IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CachedTab {
    pub action: Action,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Identity {
    workspace: WorkspaceId,
    action: Action,
}

struct Row {
    identity: Identity,
    path: PathBuf,
    label: String,
    context: String,
    detail: String,
}

fn row(slot: &WorkspaceSlot, action: Action, label: &str, context: &str, detail: &str) -> Row {
    Row {
        identity: Identity {
            workspace: slot.workspace_id,
            action,
        },
        path: slot.path.clone(),
        label: sanitize_presentation_line(label),
        // Include the path so equally named projects remain distinguishable.
        context: sanitize_presentation_line(&format!(
            "{} / {} ({})",
            slot.label,
            context,
            slot.path.display()
        )),
        detail: sanitize_presentation_line(detail),
    }
}

fn rows(slots: &[WorkspaceSlot], query: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    for slot in slots {
        rows.push(row(
            slot,
            Action::Workspace,
            &format!("Project · {}", slot.label),
            "workspace",
            "Open project",
        ));
        for session in &slot.sessions {
            let session = &session.projected;
            if session.lifecycle != SessionLifecycle::Available {
                continue;
            }
            rows.push(row(
                slot,
                Action::Session(session.id),
                &format!("Session · {}", session.label),
                &session.label,
                "Open session",
            ));
            for command in closeup::commands().filter(|command| command.name != "diff") {
                // Diff currently has no executable command; use the same command
                // registry for every operation that the Closeup can execute.
                rows.push(row(
                    slot,
                    Action::Command {
                        session: Some(session.id),
                        name: command.name,
                    },
                    &format!("Command · {}", command.name),
                    &session.label,
                    command.description,
                ));
            }
        }
        for tab in &slot.palette_tabs {
            let context = match tab.action.session() {
                Some(id) => {
                    let Some(session) = slot.sessions.iter().find(|session| {
                        session.projected.id == id
                            && session.projected.lifecycle == SessionLifecycle::Available
                    }) else {
                        continue;
                    };
                    session.projected.label.as_str()
                }
                None => "workspace",
            };
            rows.push(row(
                slot,
                tab.action.clone(),
                &tab.label,
                context,
                "Open observed tab; availability is checked on entry",
            ));
        }
        for command in overview::commands() {
            rows.push(row(
                slot,
                Action::Command {
                    session: None,
                    name: command.name,
                },
                &format!("Command · {}", command.name),
                "workspace",
                command.description,
            ));
        }
    }
    let mut matched = rows
        .into_iter()
        .filter_map(|row| {
            let text = format!("{} {} {}", row.label, row.context, row.detail);
            let score = query.split_whitespace().try_fold(0_usize, |score, token| {
                fuzzy_score(&text, token).map(|next| score.saturating_add(next))
            })?;
            Some((score, row))
        })
        .collect::<Vec<_>>();
    matched.sort_by_key(|(score, _)| *score);
    matched.into_iter().map(|(_, row)| row).collect()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Palette {
    input: TextInput,
    selected: Option<Identity>,
    unavailable: bool,
}

impl Palette {
    fn select_first(&mut self, slots: &[WorkspaceSlot]) {
        self.selected = rows(slots, self.input.value())
            .first()
            .map(|row| row.identity.clone());
        self.unavailable = false;
    }

    pub(super) fn handle(
        &mut self,
        key: &Key,
        slots: &[WorkspaceSlot],
        page: usize,
    ) -> OverlayIntent {
        let rows = rows(slots, self.input.value());
        let index = rows
            .iter()
            .position(|row| Some(&row.identity) == self.selected.as_ref());
        match key {
            Key::Escape | Key::Quit => return OverlayIntent::Cancel,
            Key::Enter => {
                if let Some(row) = index.and_then(|index| rows.get(index)) {
                    return OverlayIntent::PaletteVisit {
                        path: row.path.clone(),
                        workspace: row.identity.workspace,
                        action: row.identity.action.clone(),
                    };
                }
                self.unavailable = self.selected.is_some();
            }
            Key::Up | Key::Down | Key::PageUp | Key::PageDown => {
                let offset = if matches!(key, Key::PageUp | Key::PageDown) {
                    page
                } else {
                    1
                };
                let next = if matches!(key, Key::Up | Key::PageUp) {
                    index.unwrap_or(0).saturating_sub(offset)
                } else {
                    index
                        .map_or(0, |index| index.saturating_add(offset))
                        .min(rows.len().saturating_sub(1))
                };
                self.selected = rows.get(next).map(|row| row.identity.clone());
                self.unavailable = false;
            }
            Key::Char(character) => {
                if self.input.value().chars().count() < MAX_QUERY_CHARS {
                    self.input.insert(*character);
                }
                self.select_first(slots);
            }
            Key::Paste(text) => {
                let remaining = MAX_QUERY_CHARS.saturating_sub(self.input.value().chars().count());
                self.input
                    .insert_str(&text.chars().take(remaining).collect::<String>());
                self.select_first(slots);
            }
            Key::Backspace => {
                self.input.backspace();
                self.select_first(slots);
            }
            Key::Delete => {
                self.input.delete_forward();
                self.select_first(slots);
            }
            Key::Left => self.input.move_left(),
            Key::Right => self.input.move_right(),
            Key::Home | Key::LineStart => self.input.move_home(),
            Key::End | Key::LineEnd => self.input.move_end(),
            _ => {}
        }
        OverlayIntent::Stay
    }
}

pub(super) fn page_size(height: usize) -> usize {
    height.saturating_sub(11).clamp(1, 10)
}

pub(super) fn render(
    deck: &WorkspaceDeck,
    palette: &Palette,
    height: usize,
    width: usize,
    base: &[String],
) -> Vec<String> {
    let inner = modal::modal_inner_width(width, 92);
    let rows = rows(&deck.slots, palette.input.value());
    let index = rows
        .iter()
        .position(|row| Some(&row.identity) == palette.selected.as_ref());
    let selected = index.and_then(|index| rows.get(index));
    let capacity = modal::reserved_body_height(height, width, 16);
    let visible = page_size(height).min(capacity.saturating_sub(2)).max(1);
    let (start, end) = modal::list_window(rows.len(), index.unwrap_or(0), visible);
    let mut body = vec![modal::filter_line(
        palette.input.value(),
        palette.input.cursor(),
        palette.input.selection(),
    )];
    if rows.is_empty() {
        body.push("No matching sessions, tabs or commands".to_owned());
    }
    for (position, row) in rows.iter().enumerate().take(end).skip(start) {
        let text = format!(
            "{} {} — {}",
            modal::selection_marker(index == Some(position)),
            row.label,
            row.context
        );
        body.push(if index == Some(position) {
            Role::Accent.style().bold().paint(&text)
        } else {
            text
        });
    }
    let notice = deck.notice().or_else(|| {
        (palette.unavailable || (palette.selected.is_some() && index.is_none()))
            .then_some("Selection is no longer available; choose again.")
    });
    if body.len() + 1 < capacity {
        if let Some(notice) = notice {
            body.push(modal::error_line(notice, inner));
        } else if let Some(selected) = selected {
            body.push(selected.detail.clone());
        }
    }
    if body.len() < capacity {
        body.push(modal::footer("Enter open · Esc cancel · ↑↓ / PgUp PgDn"));
    }
    modal::render_body_over(
        height,
        width,
        base,
        "Find sessions, tabs and commands",
        inner,
        body.len(),
        body,
    )
}

impl WorkspaceDeck {
    pub(crate) fn palette_overlay_open(&self) -> bool {
        matches!(self.overlay, Some(DeckOverlay::Palette(_)))
    }

    pub(crate) fn open_command_palette(&mut self) {
        let mut palette = Palette::default();
        palette.select_first(&self.slots);
        self.overlay = Some(DeckOverlay::Palette(palette));
        self.notice = None;
    }

    /// Select a first result when asynchronous observation fills an empty list.
    /// Capture its identity before drawing; a vanished selection never falls
    /// through to a different result on the next Enter.
    pub(crate) fn refresh_command_palette_selection(&mut self) -> bool {
        let Some(DeckOverlay::Palette(palette)) = self.overlay.as_mut() else {
            return false;
        };
        if palette.selected.is_some() {
            return false;
        }
        palette.select_first(&self.slots);
        palette.selected.is_some()
    }

    pub(crate) fn update_palette_tabs(&mut self, runtime: &WorkspaceRuntime) {
        if let Some(slot) = self
            .slots
            .iter_mut()
            .find(|slot| slot.workspace_id == runtime.state().workspace())
        {
            slot.palette_tabs = runtime.palette_tabs();
        }
    }

    pub(crate) fn schedule_palette_visit(
        &mut self,
        path: PathBuf,
        workspace: WorkspaceId,
        action: Action,
    ) {
        self.pending_palette = Some((path, workspace, action));
    }

    pub(crate) fn take_palette_visit(&mut self, path: &Path) -> Option<(WorkspaceId, Action)> {
        if self
            .pending_palette
            .as_ref()
            .is_some_and(|(pending, _, _)| pending == path)
        {
            self.pending_palette
                .take()
                .map(|(_, workspace, action)| (workspace, action))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests;

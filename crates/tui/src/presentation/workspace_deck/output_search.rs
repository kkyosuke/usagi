//! Snapshot search owned by the human TUI. Every selection retains its identity.
use super::{DeckOverlay, Key, OverlayIntent, PathBuf, WorkspaceDeck, WorkspaceId, modal};
use crate::presentation::widgets::TextInput;
use crate::usecase::application::{
    controller::Target,
    pane::{PaneKind, TabSelection},
};
use std::sync::Arc;
use usagi_core::domain::presentation_text::sanitize_presentation_line;
use usagi_core::usecase::terminal_search::{
    OutputMatch, QUERY_MAX, SearchTerminalKind, TerminalSearchResult, WorkspaceOutputSearch,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub path: PathBuf,
    pub source: Arc<TerminalSearchResult>,
    pub found: OutputMatch,
    pub query: String,
    label: String,
}

impl Hit {
    pub(crate) fn action(&self) -> super::palette::Action {
        let terminal = &self.source.terminal;
        super::palette::Action::Tab {
            target: terminal
                .session_id
                .map_or(Target::Root(terminal.workspace_id), Target::Session),
            selection: TabSelection::Live(terminal.clone()),
            kind: match self.source.kind {
                SearchTerminalKind::Agent => PaneKind::Agent,
                SearchTerminalKind::Terminal => PaneKind::Terminal,
            },
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Search {
    input: TextInput,
    epoch: u64,
    revision: u64,
    requested: Option<u64>,
    hits: Vec<Hit>,
    cursor: usize,
    status: String,
}

impl Search {
    pub(super) fn handle(&mut self, key: &Key) -> OverlayIntent {
        let before = self.input.value().to_owned();
        match key {
            Key::Escape | Key::Quit => return OverlayIntent::Cancel,
            Key::Enter => {
                if let Some(hit) = self.hits.get(self.cursor) {
                    return OverlayIntent::OutputVisit(Box::new(hit.clone()));
                }
            }
            Key::Up | Key::PageUp => {
                self.cursor =
                    self.cursor
                        .saturating_sub(if matches!(key, Key::PageUp) { 5 } else { 1 });
            }
            Key::Down | Key::PageDown => {
                self.cursor = self
                    .cursor
                    .saturating_add(if matches!(key, Key::PageDown) { 5 } else { 1 })
                    .min(self.hits.len().saturating_sub(1));
            }
            Key::Char(ch) if !ch.is_control() && self.input.value().chars().count() < QUERY_MAX => {
                self.input.insert(*ch);
            }
            Key::Paste(text) => {
                let remaining = QUERY_MAX.saturating_sub(self.input.value().chars().count());
                self.input.insert_str(
                    &text
                        .chars()
                        .filter(|ch| !ch.is_control())
                        .take(remaining)
                        .collect::<String>(),
                );
            }
            Key::Backspace => {
                self.input.backspace();
            }
            Key::Delete => {
                self.input.delete_forward();
            }
            Key::Left => self.input.move_left(),
            Key::Right => self.input.move_right(),
            Key::Home | Key::LineStart => self.input.move_home(),
            Key::End | Key::LineEnd => self.input.move_end(),
            _ => {}
        }
        if before != self.input.value() || matches!(key, Key::Tab) {
            self.revision = self.revision.wrapping_add(1);
            self.hits.clear();
            self.cursor = 0;
            self.status = if self.input.value().is_empty() {
                "Type to search retained output"
            } else {
                "Searching…"
            }
            .into();
        }
        OverlayIntent::Stay
    }
}

pub(crate) type SearchToken = (u64, u64);
pub(crate) type SearchResponses = Vec<(WorkspaceId, Result<WorkspaceOutputSearch, String>)>;

impl WorkspaceDeck {
    pub(crate) fn open_output_search(&mut self) {
        self.search_epoch = self.search_epoch.wrapping_add(1);
        self.overlay = Some(DeckOverlay::OutputSearch(Search {
            epoch: self.search_epoch,
            status: "Type to search retained output".into(),
            ..Search::default()
        }));
        self.pending_output_hit = None;
        self.notice = None;
    }

    pub(crate) fn output_search_token(&self) -> Option<SearchToken> {
        match &self.overlay {
            Some(DeckOverlay::OutputSearch(search)) => Some((search.epoch, search.revision)),
            _ => None,
        }
    }

    pub(crate) fn take_output_search(&mut self) -> Option<(SearchToken, String, Vec<WorkspaceId>)> {
        let Some(DeckOverlay::OutputSearch(search)) = &mut self.overlay else {
            return None;
        };
        if search.input.value().is_empty() || search.requested == Some(search.revision) {
            return None;
        }
        search.requested = Some(search.revision);
        Some((
            (search.epoch, search.revision),
            search.input.value().to_owned(),
            self.slots.iter().map(|slot| slot.workspace_id).collect(),
        ))
    }

    pub(crate) fn apply_output_search(
        &mut self,
        token: SearchToken,
        responses: SearchResponses,
    ) -> bool {
        let Some(DeckOverlay::OutputSearch(search)) = &mut self.overlay else {
            return false;
        };
        if (search.epoch, search.revision) != token {
            return false;
        }
        let mut unavailable = 0;
        let mut limited = 0;
        let mut scanned = 0;
        let mut projects = 0;
        for (workspace, response) in responses {
            let Some(slot) = self
                .slots
                .iter()
                .find(|slot| slot.workspace_id == workspace)
            else {
                continue;
            };
            let Ok(response) = response else {
                unavailable += 1;
                continue;
            };
            if response.workspace != workspace || response.query != search.input.value() {
                unavailable += 1;
                continue;
            }
            projects += 1;
            limited += response.omitted_terminals;
            for result in response.terminals {
                if result.terminal.workspace_id != workspace {
                    unavailable += 1;
                    continue;
                }
                unavailable += usize::from(result.unavailable);
                limited += usize::from(result.limited);
                scanned += result.scanned_rows;
                let session = result
                    .terminal
                    .session_id
                    .and_then(|id| {
                        slot.sessions
                            .iter()
                            .find(|session| session.projected.id == id)
                    })
                    .map_or("workspace", |session| session.projected.label.as_str());
                let label = sanitize_presentation_line(&format!(
                    "{} / {} / {:?} {}{}",
                    slot.label,
                    session,
                    result.kind,
                    result
                        .terminal
                        .terminal_id
                        .as_str()
                        .chars()
                        .take(8)
                        .collect::<String>(),
                    if result.exited { " (exited)" } else { "" }
                ));
                let source = Arc::new(result);
                for found in &source.matches {
                    search.hits.push(Hit {
                        path: slot.path.clone(),
                        source: Arc::clone(&source),
                        found: found.clone(),
                        query: search.input.value().to_owned(),
                        label: label.clone(),
                    });
                }
            }
        }
        search.status = format!(
            "Snapshot · {projects}/{} projects · {} hits · {scanned} rows · {unavailable} unavailable · {limited} limited/omitted",
            self.slots.len(),
            search.hits.len()
        );
        true
    }
}

pub(super) fn render(
    deck: &WorkspaceDeck,
    search: &Search,
    height: usize,
    width: usize,
    base: &[String],
) -> Vec<String> {
    let inner = modal::modal_inner_width(width, 100);
    let mut body = vec![
        modal::filter_line(
            search.input.value(),
            search.input.cursor(),
            search.input.selection(),
        ),
        search.status.clone(),
        "Caps: 16 projects; each 64 terminals / 20k rows / 2 MiB / 100 hits".into(),
    ];
    let visible = height.saturating_sub(15).clamp(1, 8);
    let (start, end) = modal::list_window(search.hits.len(), search.cursor, visible);
    for (index, hit) in search.hits.iter().enumerate().take(end).skip(start) {
        body.push(format!(
            "{} {}:{} · {}",
            modal::selection_marker(index == search.cursor),
            hit.label,
            hit.found.row + 1,
            sanitize_presentation_line(&hit.found.text)
        ));
    }
    if let Some(hit) = search.hits.get(search.cursor) {
        body.push(sanitize_presentation_line(&format!(
            "{} · retained {} rows, scanned {}",
            hit.path.display(),
            hit.source.retained_rows,
            hit.source.scanned_rows,
        )));
        body.extend([
            format!("  {}", sanitize_presentation_line(&hit.found.before)),
            format!("> {}", sanitize_presentation_line(&hit.found.text)),
            format!("  {}", sanitize_presentation_line(&hit.found.after)),
        ]);
    }
    if let Some(notice) = deck.notice() {
        body.push(modal::error_line(notice, inner));
    }
    body.push(modal::footer(
        "Enter open match · Tab refresh · Esc cancel · ↑↓ / PgUp PgDn",
    ));
    modal::render_body_over(
        height,
        width,
        base,
        "Search terminal output (retained rows)",
        inner,
        body.len(),
        body,
    )
}

#[cfg(test)]
mod tests;

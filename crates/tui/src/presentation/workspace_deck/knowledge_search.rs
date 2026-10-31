//! Issue / Memory picker and exact-source reader, with stale response fences.
use super::{DeckOverlay, Key, OverlayIntent, WorkspaceDeck, modal};
use crate::presentation::knowledge_search::{Request, Response};
use crate::presentation::widgets::TextInput;
use crate::usecase::application::knowledge_search::{Document, Kind, QUERY_LIMIT, Results};
use crate::usecase::terminal_input::LiveTerminalAction;
use usagi_core::domain::presentation_text::sanitize_presentation_line;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Search {
    input: TextInput,
    epoch: u64,
    revision: u64,
    requested: Option<u64>,
    results: Results,
    filter: usize,
    cursor: usize,
    document: Option<Document>,
    lines: Vec<String>,
    scroll: usize,
    horizontal: usize,
    status: String,
}

impl Search {
    fn rows(&self) -> Vec<&Document> {
        self.results
            .documents
            .iter()
            .filter(|doc| match self.filter {
                1 => doc.kind == Kind::Issue,
                2 => doc.kind == Kind::Memory,
                _ => true,
            })
            .collect()
    }

    fn refresh(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.results = Results::default();
        self.document = None;
        self.lines.clear();
        self.cursor = 0;
        self.status = "Searching…".into();
    }

    pub(super) fn handle(&mut self, key: &Key) -> OverlayIntent {
        if matches!(key, Key::Live(LiveTerminalAction::OpenKnowledgeSearch)) {
            self.refresh();
            return OverlayIntent::Stay;
        }
        if self.document.is_some() {
            match key {
                Key::Escape => {
                    self.document = None;
                    self.lines.clear();
                    self.revision = self.revision.wrapping_add(1);
                    self.requested = Some(self.revision);
                    self.status = self.summary();
                }
                Key::Quit => return OverlayIntent::Cancel,
                Key::Up | Key::PageUp => {
                    self.scroll =
                        self.scroll
                            .saturating_sub(if matches!(key, Key::Up) { 1 } else { 10 });
                }
                Key::Down | Key::PageDown => {
                    self.scroll = self
                        .scroll
                        .saturating_add(if matches!(key, Key::Down) { 1 } else { 10 })
                        .min(self.lines.len().saturating_sub(1));
                }
                Key::Home => self.scroll = 0,
                Key::End => self.scroll = self.lines.len().saturating_sub(1),
                Key::Left => self.horizontal = self.horizontal.saturating_sub(10),
                Key::Right => self.horizontal = self.horizontal.saturating_add(10).min(512 * 1024),
                _ => {}
            }
            return OverlayIntent::Stay;
        }
        let mut query_changed = false;
        match key {
            Key::Escape | Key::Quit => return OverlayIntent::Cancel,
            Key::Enter => {
                if let Some(document) = self.rows().get(self.cursor) {
                    self.document = Some((*document).clone());
                    self.revision = self.revision.wrapping_add(1);
                    self.scroll = 0;
                    self.horizontal = 0;
                    self.status = "Loading document…".into();
                }
            }
            Key::Tab => {
                self.filter = (self.filter + 1) % 3;
                self.cursor = 0;
            }
            Key::Up | Key::PageUp => {
                self.cursor =
                    self.cursor
                        .saturating_sub(if matches!(key, Key::Up) { 1 } else { 5 });
            }
            Key::Down | Key::PageDown => {
                self.cursor = self
                    .cursor
                    .saturating_add(if matches!(key, Key::Down) { 1 } else { 5 })
                    .min(self.rows().len().saturating_sub(1));
            }
            _ => query_changed = super::search_input::edit(&mut self.input, key, QUERY_LIMIT),
        }
        if query_changed {
            self.refresh();
        }
        OverlayIntent::Stay
    }

    fn summary(&self) -> String {
        format!(
            "{} results · {} omitted · {} unavailable (max 100 per store)",
            self.results.documents.len(),
            self.results.omitted,
            self.results.errors.len()
        )
    }
}

impl WorkspaceDeck {
    pub(crate) fn open_knowledge_search(&mut self) {
        self.notice = None;
        if let Some(DeckOverlay::Knowledge(search)) = &mut self.overlay {
            search.refresh();
            return;
        }
        self.knowledge_epoch = self.knowledge_epoch.wrapping_add(1);
        self.overlay = Some(DeckOverlay::Knowledge(Search {
            epoch: self.knowledge_epoch,
            status: "Searching…".into(),
            ..Search::default()
        }));
    }

    pub(crate) fn knowledge_search_token(&self) -> Option<(u64, u64)> {
        match &self.overlay {
            Some(DeckOverlay::Knowledge(search)) => Some((search.epoch, search.revision)),
            _ => None,
        }
    }

    pub(crate) fn take_knowledge_request(&mut self) -> Option<((u64, u64), Request)> {
        let Some(DeckOverlay::Knowledge(search)) = &mut self.overlay else {
            return None;
        };
        if search.requested == Some(search.revision) {
            return None;
        }
        search.requested = Some(search.revision);
        let request = search.document.as_ref().map_or_else(
            || Request::Search(search.input.value().to_owned()),
            |document| Request::Read(document.path.clone()),
        );
        Some(((search.epoch, search.revision), request))
    }

    pub(crate) fn apply_knowledge_response(
        &mut self,
        token: (u64, u64),
        response: Response,
    ) -> bool {
        let Some(DeckOverlay::Knowledge(search)) = &mut self.overlay else {
            return false;
        };
        if (search.epoch, search.revision) != token {
            return false;
        }
        match response {
            Response::Search(results) if search.document.is_none() => {
                search.results = results;
                search.status = search.summary();
            }
            Response::Read(result) if search.document.is_some() => match result {
                Ok(lines) => {
                    let needle = search.input.value().to_lowercase();
                    search.scroll = lines
                        .iter()
                        .position(|line| line.to_lowercase().contains(&needle))
                        .unwrap_or(0);
                    search.status = format!("{} lines · current file (max 512 KiB)", lines.len());
                    search.lines = lines;
                }
                Err(error) => search.status = error,
            },
            _ => return false,
        }
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
    let inner = modal::modal_inner_width(width, 110);
    let mut body = vec![
        sanitize_presentation_line(&format!("Workspace: {}", deck.active_path().display())),
        sanitize_presentation_line(&search.status),
    ];
    let visible = height.saturating_sub(14).clamp(1, 25);
    if let Some(document) = &search.document {
        body.push(sanitize_presentation_line(&document.title));
        body.push(sanitize_presentation_line(&document.path));
        body.extend(
            search
                .lines
                .iter()
                .enumerate()
                .skip(search.scroll)
                .take(visible)
                .map(|(index, line)| {
                    format!(
                        "{}: {}",
                        index + 1,
                        sanitize_presentation_line(line)
                            .chars()
                            .skip(search.horizontal)
                            .collect::<String>()
                    )
                }),
        );
        body.push(modal::footer(
            "Esc results · ↑↓ / PgUp PgDn scroll · ←→ pan · Ctrl-O m refresh",
        ));
    } else {
        body.push(modal::filter_line(
            search.input.value(),
            search.input.cursor(),
            search.input.selection(),
        ));
        body.push(format!(
            "{} · Tab filter · Ctrl-O m refresh",
            ["All", "Issues", "Memory"][search.filter]
        ));
        body.extend(
            search
                .results
                .errors
                .iter()
                .map(|error| modal::error_line(&sanitize_presentation_line(error), inner)),
        );
        let rows = search.rows();
        let (start, end) = modal::list_window(rows.len(), search.cursor, visible);
        if rows.is_empty()
            && search.requested == Some(search.revision)
            && search.status != "Searching…"
        {
            body.push("No matching documents in this filter".into());
        }
        for (index, document) in rows.iter().enumerate().take(end).skip(start) {
            body.push(sanitize_presentation_line(&format!(
                "{} {:?} {} · {}",
                modal::selection_marker(index == search.cursor),
                document.kind,
                document.title,
                document.detail
            )));
        }
        if let Some(document) = rows.get(search.cursor) {
            body.push(sanitize_presentation_line(&document.path));
        }
        body.push(modal::footer(
            "Enter read · Esc close · ↑↓ / PgUp PgDn select",
        ));
    }
    modal::render_body_over(
        height,
        width,
        base,
        "Search Issue / Memory",
        inner,
        body.len(),
        body,
    )
}

#[cfg(test)]
mod tests;

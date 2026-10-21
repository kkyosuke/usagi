//! One query, explicit source scopes, and stable result identities.
use super::{
    DeckOverlay, Key, OverlayIntent, WorkspaceDeck, WorkspaceSlot, modal, output_search, palette,
};
use crate::presentation::unified_search::{Request, Response};
use crate::presentation::widgets::TextInput;
use crate::usecase::application::{
    controller::Target,
    knowledge_search::{Document, QUERY_LIMIT},
    unified_search::{Artifact, CurrentSession, Project, Query, RESULT_LIMIT, Scope},
};
use usagi_core::domain::{id::SessionId, presentation_text::sanitize_presentation_line};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    Navigation(palette::Row),
    Output(output_search::Hit),
    Artifact(Artifact),
    Document(Document),
}

impl Entry {
    fn label(&self) -> String {
        match self {
            Self::Navigation(row) => format!("{} · {}", row.label, row.context),
            Self::Output(hit) => format!(
                "Terminal · {}:{} · {}",
                hit.label,
                hit.found.row + 1,
                hit.found.text
            ),
            Self::Artifact(file) => {
                format!("Artifact · {} · {}", file.project_root.display(), file.path)
            }
            Self::Document(doc) => format!("{:?} · {} · {}", doc.kind, doc.title, doc.detail),
        }
    }

    fn detail(&self) -> String {
        match self {
            Self::Navigation(row) => row.detail.clone(),
            Self::Output(hit) => format!(
                "{} | {} | {}",
                hit.found.before, hit.found.text, hit.found.after
            ),
            Self::Artifact(file) => format!("{} · Enter opens the artifact viewer", file.path),
            Self::Document(doc) => doc.path.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Search {
    input: TextInput,
    epoch: u64,
    revision: u64,
    requested: Option<u64>,
    completed: Option<u64>,
    navigation_omitted: usize,
    source_omitted: usize,
    scope: Scope,
    current: Option<CurrentSession>,
    entries: Vec<Entry>,
    selected: Option<Entry>,
    status: String,
    output_status: String,
    errors: Vec<String>,
    document: Option<Document>,
    lines: Vec<String>,
    scroll: usize,
    horizontal: usize,
}

impl Search {
    fn refresh(&mut self, slots: &[WorkspaceSlot]) {
        self.revision = self.revision.wrapping_add(1);
        self.document = None;
        self.lines.clear();
        self.errors.clear();
        self.output_status.clear();
        self.entries.clear();
        self.navigation_omitted = 0;
        self.source_omitted = 0;
        self.refresh_navigation(slots);
        self.selected = self.entries.first().cloned();
        self.status = "Searching…".into();
    }

    fn refresh_navigation(&mut self, slots: &[WorkspaceSlot]) {
        if !matches!(self.scope, Scope::All | Scope::CurrentSession) {
            return;
        }
        let retained = self
            .entries
            .iter()
            .filter(|entry| !matches!(entry, Entry::Navigation(_)))
            .cloned()
            .collect::<Vec<_>>();
        self.entries = palette::rows(slots, self.input.value())
            .into_iter()
            .filter(|row| {
                self.scope == Scope::All
                    || self.current.as_ref().is_some_and(|current| {
                        current.workspace == row.identity.workspace
                            && row.identity.action.session() == Some(current.session)
                    })
            })
            .map(Entry::Navigation)
            .collect();
        self.navigation_omitted = self.entries.len().saturating_sub(RESULT_LIMIT);
        self.entries.truncate(RESULT_LIMIT);
        self.entries.extend(retained);
        if self.completed == Some(self.revision) {
            self.update_status();
        }
        if self.selected.is_none() {
            self.selected = self.entries.first().cloned();
        }
    }

    fn update_status(&mut self) {
        self.status = format!(
            "{} results · {} omitted · {} unavailable",
            self.entries.len(),
            self.source_omitted + self.navigation_omitted,
            self.errors.len()
        );
    }

    fn index(&self) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| Some(entry) == self.selected.as_ref())
    }

    fn change_scope(&mut self, slots: &[WorkspaceSlot], reverse: bool) {
        self.scope = self.scope.next();
        if reverse {
            self.scope = self.scope.next().next();
        }
        self.refresh(slots);
    }

    pub(super) fn handle(&mut self, key: &Key, slots: &[WorkspaceSlot]) -> OverlayIntent {
        if matches!(key, Key::Tab) {
            self.change_scope(slots, false);
            return OverlayIntent::Stay;
        }
        if matches!(key, Key::Passthrough(bytes) if bytes == b"\x1b[Z") {
            self.change_scope(slots, true);
            return OverlayIntent::Stay;
        }
        if self.document.is_some() {
            return self.handle_document(key);
        }
        let before = self.input.value().to_owned();
        match key {
            Key::Escape | Key::Quit => return OverlayIntent::Cancel,
            Key::Enter => {
                if let Some(entry) = self.index().map(|index| self.entries[index].clone()) {
                    match entry {
                        Entry::Navigation(row) => {
                            return OverlayIntent::PaletteVisit {
                                path: row.path,
                                workspace: row.identity.workspace,
                                action: row.identity.action,
                            };
                        }
                        Entry::Output(hit) => return OverlayIntent::OutputVisit(Box::new(hit)),
                        Entry::Artifact(file) => {
                            return OverlayIntent::PaletteVisit {
                                path: file.project_root,
                                workspace: file.workspace,
                                action: palette::Action::Artifact {
                                    target: file.target,
                                    path: file.path,
                                },
                            };
                        }
                        Entry::Document(doc) => {
                            self.document = Some(doc);
                            self.revision = self.revision.wrapping_add(1);
                            self.scroll = 0;
                            self.horizontal = 0;
                            self.status = "Loading document…".into();
                        }
                    }
                }
            }
            Key::Up | Key::Down | Key::PageUp | Key::PageDown => {
                let step = if matches!(key, Key::PageUp | Key::PageDown) {
                    5
                } else {
                    1
                };
                let index = self.index().unwrap_or(0);
                let next = if matches!(key, Key::Up | Key::PageUp) {
                    index.saturating_sub(step)
                } else {
                    index
                        .saturating_add(step)
                        .min(self.entries.len().saturating_sub(1))
                };
                self.selected = self.entries.get(next).cloned();
            }
            Key::Char(ch)
                if !ch.is_control() && self.input.value().chars().count() < QUERY_LIMIT =>
            {
                self.input.insert(*ch);
            }
            Key::Paste(text) => {
                let remaining = QUERY_LIMIT.saturating_sub(self.input.value().chars().count());
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
        if before != self.input.value() {
            self.refresh(slots);
        }
        OverlayIntent::Stay
    }

    fn handle_document(&mut self, key: &Key) -> OverlayIntent {
        match key {
            Key::Escape => {
                self.document = None;
                self.lines.clear();
                self.revision = self.revision.wrapping_add(1);
                self.requested = Some(self.revision);
                self.status = format!("{} results", self.entries.len());
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
        OverlayIntent::Stay
    }
}

impl WorkspaceDeck {
    pub(crate) fn open_unified_search(&mut self, session: Option<SessionId>) {
        self.notice = None;
        self.pending_output_hit = None;
        if let Some(DeckOverlay::Unified(search)) = &mut self.overlay {
            search.refresh(&self.slots);
            return;
        }
        self.unified_epoch = self.unified_epoch.wrapping_add(1);
        let current = self
            .slots
            .iter()
            .find(|slot| slot.workspace_id == self.active)
            .and_then(|slot| {
                slot.sessions
                    .iter()
                    .find(|item| Some(item.projected.id) == session)
                    .map(|item| CurrentSession {
                        workspace: slot.workspace_id,
                        session: item.projected.id,
                        root: item.projected.cwd.clone(),
                    })
            });
        let mut search = Search {
            epoch: self.unified_epoch,
            current,
            ..Search::default()
        };
        search.refresh(&self.slots);
        self.overlay = Some(DeckOverlay::Unified(Box::new(search)));
    }

    pub(crate) fn refresh_unified_navigation(&mut self) -> bool {
        let Some(DeckOverlay::Unified(search)) = &mut self.overlay else {
            return false;
        };
        if search.document.is_some() {
            return false;
        }
        let before = search.entries.clone();
        search.refresh_navigation(&self.slots);
        before != search.entries
    }

    pub(crate) fn unified_search_token(&self) -> Option<(u64, u64)> {
        match &self.overlay {
            Some(DeckOverlay::Unified(search)) => Some((search.epoch, search.revision)),
            _ => None,
        }
    }

    pub(crate) fn take_unified_request(&mut self) -> Option<((u64, u64), Request)> {
        let Some(DeckOverlay::Unified(search)) = &mut self.overlay else {
            return None;
        };
        if search.requested == Some(search.revision) {
            return None;
        }
        search.requested = Some(search.revision);
        let request = search.document.as_ref().map_or_else(
            || {
                Request::Search(Query {
                    text: search.input.value().to_owned(),
                    scope: search.scope,
                    current: search.current.clone(),
                    projects: self
                        .slots
                        .iter()
                        .map(|slot| Project {
                            workspace: slot.workspace_id,
                            root: slot.path.clone(),
                        })
                        .collect(),
                })
            },
            |doc| Request::Read(doc.path.clone()),
        );
        Some(((search.epoch, search.revision), request))
    }

    pub(crate) fn apply_unified_response(&mut self, token: (u64, u64), response: Response) -> bool {
        let Some(DeckOverlay::Unified(search)) = &mut self.overlay else {
            return false;
        };
        if (search.epoch, search.revision) != token || search.completed == Some(search.revision) {
            return false;
        }
        match response {
            Response::Search(results) if search.document.is_none() => {
                let (hits, status) =
                    output_search::collect_hits(&self.slots, search.input.value(), results.outputs);
                search.output_status = status;
                search.entries.extend(
                    hits.into_iter()
                        .filter(|hit| {
                            search.scope != Scope::Artifact
                                && (search.scope != Scope::CurrentSession
                                    || search.current.as_ref().is_some_and(|current| {
                                        current.workspace == hit.source.terminal.workspace_id
                                            && hit.source.terminal.session_id
                                                == Some(current.session)
                                    }))
                        })
                        .map(Entry::Output),
                );
                search.entries.extend(
                    results
                        .artifacts
                        .into_iter()
                        .filter(|file| {
                            search.scope != Scope::Terminal
                                && self.slots.iter().any(|slot| {
                                    slot.workspace_id == file.workspace
                                        && slot.path == file.project_root
                                })
                                && (search.scope != Scope::CurrentSession
                                    || search.current.as_ref().is_some_and(|current| {
                                        current.workspace == file.workspace
                                            && file.target == Target::Session(current.session)
                                    }))
                        })
                        .map(Entry::Artifact),
                );
                search.entries.extend(
                    results
                        .knowledge
                        .documents
                        .into_iter()
                        .filter(|_| search.scope == Scope::All)
                        .map(Entry::Document),
                );
                search.errors = results.errors;
                search.errors.extend(results.knowledge.errors);
                search.source_omitted = results.omitted + results.knowledge.omitted;
                search.update_status();
                if search.selected.is_none() {
                    search.selected = search.entries.first().cloned();
                }
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
        search.completed = Some(search.revision);
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
    let scopes = [
        Scope::All,
        Scope::Terminal,
        Scope::Artifact,
        Scope::CurrentSession,
    ]
    .map(|scope| {
        if scope == search.scope {
            format!("[{}]", scope.label())
        } else {
            scope.label().into()
        }
    })
    .join(" · ");
    let mut body = vec![scopes, sanitize_presentation_line(&search.status)];
    let visible = height.saturating_sub(15).clamp(1, 16);
    if let Some(doc) = &search.document {
        body.push(sanitize_presentation_line(&doc.title));
        body.push(sanitize_presentation_line(&doc.path));
        body.extend(
            search
                .lines
                .iter()
                .enumerate()
                .skip(search.scroll)
                .take(visible)
                .map(|(i, line)| {
                    format!(
                        "{}: {}",
                        i + 1,
                        sanitize_presentation_line(line)
                            .chars()
                            .skip(search.horizontal)
                            .collect::<String>()
                    )
                }),
        );
        body.push(modal::footer(
            "Esc results · ↑↓ / PgUp PgDn scroll · ←→ pan · Tab scope",
        ));
    } else {
        body.push(modal::filter_line(
            search.input.value(),
            search.input.cursor(),
            search.input.selection(),
        ));
        if search.scope == Scope::CurrentSession {
            body.push(search.current.as_ref().map_or_else(
                || "No current session selected".into(),
                |current| format!("Current session: {}", current.root.display()),
            ));
        }
        if search.input.value().is_empty() && search.scope != Scope::Artifact {
            body.push("Type a query to search retained terminal output".into());
        }
        if !search.output_status.is_empty() {
            body.push(search.output_status.clone());
        }
        body.extend(
            search
                .errors
                .iter()
                .take(2)
                .map(|error| modal::error_line(&sanitize_presentation_line(error), inner)),
        );
        let index = search.index();
        let (start, end) = modal::list_window(search.entries.len(), index.unwrap_or(0), visible);
        if search.entries.is_empty() {
            body.push("No matching results".into());
        }
        for (i, entry) in search.entries.iter().enumerate().take(end).skip(start) {
            body.push(sanitize_presentation_line(&format!(
                "{} {}",
                modal::selection_marker(index == Some(i)),
                entry.label()
            )));
        }
        if let Some(entry) = index.and_then(|i| search.entries.get(i)) {
            body.push(sanitize_presentation_line(&entry.detail()));
        }
        body.push(modal::footer(
            "Tab / Shift-Tab scope · Enter open · Esc close · Ctrl-O f refresh · ↑↓ select",
        ));
    }
    if let Some(notice) = deck.notice() {
        body.push(modal::error_line(notice, inner));
    }
    modal::render_body_over(height, width, base, "Search", inner, body.len(), body)
}

#[cfg(test)]
mod tests;

//! Process-level project tabs shown above one active workspace Home.
//!
//! The deck owns only workspace membership, ordering, active identity, and its
//! two small overlays. Session, pane, and Agent state remain owned by the
//! active workspace controller.

use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};

use usagi_core::domain::id::WorkspaceId;
use usagi_core::domain::workspace::Workspace;

use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::{self, modal};
use crate::usecase::application::Key;
use crate::usecase::application::WorkspaceSnapshot;

const TAB_NAME_WIDTH: usize = 18;
const ADD_LABEL: &str = "+ Open";

/// Stable metadata for one open project tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSlot {
    path: PathBuf,
    workspace_id: WorkspaceId,
    label: String,
}

impl WorkspaceSlot {
    /// Project a freshly attached snapshot into deck metadata.
    #[must_use]
    pub fn from_snapshot(snapshot: &WorkspaceSnapshot) -> Self {
        Self {
            path: snapshot.workspace.path.clone(),
            workspace_id: snapshot.workspace_id,
            label: snapshot.workspace.name.clone(),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }
}

/// Which process-level modal is in front of Home.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DeckOverlay {
    Add(AddWorkspace),
    Switcher(ProjectSwitcher),
}

/// Result of reducing one key while a deck overlay is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayIntent {
    Stay,
    Cancel,
    Add(Vec<PathBuf>),
    Activate(PathBuf),
    Close(PathBuf),
}

/// Ordered tabs and the active tab identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceDeck {
    slots: Vec<WorkspaceSlot>,
    active: WorkspaceId,
    overlay: Option<DeckOverlay>,
    notice: Option<String>,
}

impl WorkspaceDeck {
    /// Start a deck with one already prepared workspace.
    #[must_use]
    pub fn new(snapshot: &WorkspaceSnapshot) -> Self {
        let slot = WorkspaceSlot::from_snapshot(snapshot);
        Self {
            active: slot.workspace_id,
            slots: vec![slot],
            overlay: None,
            notice: None,
        }
    }

    /// Start a deck from an ordered, non-empty group of prepared snapshots.
    #[must_use]
    pub fn from_snapshots(snapshots: &[WorkspaceSnapshot]) -> Option<Self> {
        let first = snapshots.first()?;
        let mut deck = Self::new(first);
        deck.append_snapshots(&snapshots[1..]);
        Some(deck)
    }

    #[must_use]
    pub fn slots(&self) -> &[WorkspaceSlot] {
        &self.slots
    }

    #[must_use]
    pub fn paths(&self) -> Vec<PathBuf> {
        self.slots.iter().map(|slot| slot.path.clone()).collect()
    }

    #[must_use]
    pub fn active_index(&self) -> usize {
        self.slots
            .iter()
            .position(|slot| slot.workspace_id == self.active)
            .unwrap_or(0)
    }

    #[must_use]
    pub fn active_path(&self) -> &Path {
        self.slots[self.active_index()].path()
    }

    #[must_use]
    pub fn path_at(&self, index: usize) -> Option<&Path> {
        self.slots.get(index).map(WorkspaceSlot::path)
    }

    #[must_use]
    pub fn contains_path(&self, path: &Path) -> bool {
        self.slots.iter().any(|slot| slot.path == path)
    }

    /// Append prepared tabs in request order, ignoring duplicate paths.
    pub fn append_snapshots(&mut self, snapshots: &[WorkspaceSnapshot]) {
        for snapshot in snapshots {
            let slot = WorkspaceSlot::from_snapshot(snapshot);
            if !self.contains_path(&slot.path) {
                self.slots.push(slot);
            }
        }
    }

    /// Commit an activation only after the target snapshot was prepared.
    pub fn activate_snapshot(&mut self, snapshot: &WorkspaceSnapshot) {
        if let Some(slot) = self
            .slots
            .iter_mut()
            .find(|slot| slot.path == snapshot.workspace.path)
        {
            slot.workspace_id = snapshot.workspace_id;
            slot.label.clone_from(&snapshot.workspace.name);
            self.active = snapshot.workspace_id;
        }
        self.overlay = None;
        self.notice = None;
    }

    /// Open the batch-add overlay over the current Home.
    pub fn open_add(&mut self, registry: &[Workspace]) {
        let open = self.slots.iter().map(|slot| slot.path.clone()).collect();
        self.overlay = Some(DeckOverlay::Add(AddWorkspace::new(registry, open)));
        self.notice = None;
    }

    /// Open the all-tab switcher with the active row selected.
    pub fn open_switcher(&mut self) {
        self.overlay = Some(DeckOverlay::Switcher(ProjectSwitcher {
            selected: self.active_index(),
        }));
        self.notice = None;
    }

    #[must_use]
    pub fn overlay_open(&self) -> bool {
        self.overlay.is_some()
    }

    pub fn close_overlay(&mut self) {
        self.overlay = None;
        self.notice = None;
    }

    pub fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(notice.into());
    }

    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Reduce one overlay key without performing IO.
    pub fn handle_overlay_key(&mut self, key: &Key) -> OverlayIntent {
        let Some(overlay) = self.overlay.as_mut() else {
            return OverlayIntent::Stay;
        };
        self.notice = None;
        match overlay {
            DeckOverlay::Add(add) => add.handle(key),
            DeckOverlay::Switcher(switcher) => switcher.handle(key, &self.slots),
        }
    }

    /// Remove one tab. The caller prepares a replacement before closing an
    /// active tab, so this reducer only commits the already-safe mutation.
    pub fn close_path(&mut self, path: &Path) {
        let Some(index) = self.slots.iter().position(|slot| slot.path == path) else {
            return;
        };
        let was_active = self.slots[index].workspace_id == self.active;
        self.slots.remove(index);
        if was_active && !self.slots.is_empty() {
            let replacement = index.min(self.slots.len() - 1);
            self.active = self.slots[replacement].workspace_id;
        }
        self.overlay = None;
        self.notice = None;
    }

    #[must_use]
    pub fn replacement_path_after_close(&self, path: &Path) -> Option<&Path> {
        let index = self.slots.iter().position(|slot| slot.path == path)?;
        if self.slots.len() <= 1 {
            return None;
        }
        let replacement = if index + 1 < self.slots.len() {
            index + 1
        } else {
            index - 1
        };
        self.slots.get(replacement).map(WorkspaceSlot::path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AddWorkspace {
    candidates: Vec<Workspace>,
    opened: HashSet<PathBuf>,
    selected: HashSet<PathBuf>,
    cursor: usize,
    filter: String,
}

impl AddWorkspace {
    fn new(registry: &[Workspace], opened: HashSet<PathBuf>) -> Self {
        Self {
            candidates: registry.to_vec(),
            opened,
            selected: HashSet::new(),
            cursor: 0,
            filter: String::new(),
        }
    }

    fn visible(&self) -> Vec<&Workspace> {
        let filter = self.filter.to_lowercase();
        self.candidates
            .iter()
            .filter(|workspace| {
                filter.is_empty() || workspace.name.to_lowercase().contains(&filter)
            })
            .collect()
    }

    fn handle(&mut self, key: &Key) -> OverlayIntent {
        match key {
            Key::Up => self.cursor = self.cursor.saturating_sub(1),
            Key::Down => {
                self.cursor = (self.cursor + 1).min(self.visible().len().saturating_sub(1));
            }
            Key::Backspace => {
                self.filter.pop();
                self.cursor = 0;
            }
            Key::Char(' ') => {
                let path = self
                    .visible()
                    .get(self.cursor)
                    .map(|workspace| workspace.path.clone());
                if let Some(path) = path.filter(|path| !self.opened.contains(path))
                    && !self.selected.remove(&path)
                {
                    self.selected.insert(path);
                }
            }
            Key::Char(character) => {
                self.filter.push(*character);
                self.cursor = 0;
            }
            Key::Paste(text) => {
                self.filter.push_str(text);
                self.cursor = 0;
            }
            Key::Enter => {
                let paths = self
                    .candidates
                    .iter()
                    .filter(|workspace| self.selected.contains(&workspace.path))
                    .map(|workspace| workspace.path.clone())
                    .collect::<Vec<_>>();
                return OverlayIntent::Add(paths);
            }
            Key::Escape => return OverlayIntent::Cancel,
            _ => {}
        }
        OverlayIntent::Stay
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectSwitcher {
    selected: usize,
}

impl ProjectSwitcher {
    fn handle(&mut self, key: &Key, slots: &[WorkspaceSlot]) -> OverlayIntent {
        match key {
            Key::Up => self.selected = self.selected.saturating_sub(1),
            Key::Down => self.selected = (self.selected + 1).min(slots.len().saturating_sub(1)),
            Key::Char(digit @ '1'..='9') => {
                let index = digit.to_digit(10).unwrap_or(1) as usize - 1;
                if let Some(slot) = slots.get(index) {
                    return OverlayIntent::Activate(slot.path.clone());
                }
            }
            Key::Enter => {
                if let Some(slot) = slots.get(self.selected) {
                    return OverlayIntent::Activate(slot.path.clone());
                }
            }
            Key::Char('x') => {
                if let Some(slot) = slots.get(self.selected) {
                    return OverlayIntent::Close(slot.path.clone());
                }
            }
            Key::Escape => return OverlayIntent::Cancel,
            _ => {}
        }
        OverlayIntent::Stay
    }
}

/// Identity-bearing target for one cell range in the project bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectBarTarget {
    Workspace(PathBuf),
    Add,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectBarHit {
    columns: Range<usize>,
    target: ProjectBarTarget,
}

/// Rendered bar and the exact hit geometry produced with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectBar {
    pub line: String,
    hits: Vec<ProjectBarHit>,
}

impl ProjectBar {
    #[must_use]
    pub fn target_at(&self, column: usize) -> Option<&ProjectBarTarget> {
        self.hits
            .iter()
            .find(|hit| hit.columns.contains(&column))
            .map(|hit| &hit.target)
    }
}

/// Build a one-row tab bar whose selected tab is always inside the visible
/// contiguous window.
#[must_use]
pub fn project_bar(deck: &WorkspaceDeck, width: usize) -> ProjectBar {
    if width == 0 || deck.slots.is_empty() {
        return ProjectBar {
            line: String::new(),
            hits: Vec::new(),
        };
    }
    let active = deck.active_index();
    let labels = deck
        .slots
        .iter()
        .enumerate()
        .map(|(index, slot)| {
            let name = widgets::clip_to_width(slot.label(), TAB_NAME_WIDTH);
            format!(" {} {name} ", index + 1)
        })
        .collect::<Vec<_>>();
    let add_width = widgets::display_width(ADD_LABEL) + 2;

    let mut best = (active, active + 1);
    let mut best_count = 1;
    for start in 0..=active {
        for end in (active + 1)..=labels.len() {
            let used = project_bar_window_width(&labels, start, end, add_width);
            let count = end - start;
            if used <= width && count > best_count {
                best = (start, end);
                best_count = count;
            }
        }
    }

    let (start, end) = best;
    let required = project_bar_window_width(&labels, start, end, add_width);
    if required > width {
        let visible = widgets::clip_to_width(&labels[active], width);
        let span = widgets::display_width(&visible);
        return ProjectBar {
            line: Role::Accent.style().bold().paint(&visible),
            hits: vec![ProjectBarHit {
                columns: 0..span,
                target: ProjectBarTarget::Workspace(deck.slots[active].path.clone()),
            }],
        };
    }
    let mut line = String::new();
    let mut hits = Vec::new();
    let mut column = 0;
    if start > 0 {
        let hidden = format!("… +{start} ");
        column += widgets::display_width(&hidden);
        line.push_str(&Style::new().dim().paint(&hidden));
    }
    for (index, label) in labels.iter().enumerate().take(end).skip(start) {
        let available = width.saturating_sub(column + add_width);
        let visible = widgets::clip_to_width(label, available);
        let span = widgets::display_width(&visible);
        let styled = if index == active {
            Role::Accent.style().bold().paint(&visible)
        } else {
            Style::new().dim().paint(&visible)
        };
        line.push_str(&styled);
        hits.push(ProjectBarHit {
            columns: column..column + span,
            target: ProjectBarTarget::Workspace(deck.slots[index].path.clone()),
        });
        column += span;
    }
    if end < labels.len() && column < width.saturating_sub(add_width) {
        let hidden = format!("… +{} ", labels.len() - end);
        let hidden = widgets::clip_to_width(&hidden, width.saturating_sub(column + add_width));
        column += widgets::display_width(&hidden);
        line.push_str(&Style::new().dim().paint(&hidden));
    }
    if column < width {
        let add = widgets::clip_to_width(&format!(" {ADD_LABEL} "), width.saturating_sub(column));
        let span = widgets::display_width(&add);
        line.push_str(&Role::Accent.style().paint(&add));
        hits.push(ProjectBarHit {
            columns: column..column + span,
            target: ProjectBarTarget::Add,
        });
    }
    ProjectBar { line, hits }
}

fn project_bar_window_width(
    labels: &[String],
    start: usize,
    end: usize,
    add_width: usize,
) -> usize {
    let tabs = labels[start..end]
        .iter()
        .map(|label| widgets::display_width(label))
        .sum::<usize>();
    let hidden_left = if start > 0 {
        widgets::display_width(&format!("… +{start} "))
    } else {
        0
    };
    let hidden_right = if end < labels.len() {
        widgets::display_width(&format!("… +{} ", labels.len() - end))
    } else {
        0
    };
    tabs + hidden_left + hidden_right + add_width
}

/// Composite the open deck overlay over a Home frame.
#[must_use]
pub fn render_overlay(
    deck: &WorkspaceDeck,
    height: usize,
    width: usize,
    base: &[String],
) -> Vec<String> {
    match &deck.overlay {
        None => base.to_vec(),
        Some(DeckOverlay::Add(add)) => render_add(deck, add, height, width, base),
        Some(DeckOverlay::Switcher(switcher)) => {
            render_switcher(deck, switcher, height, width, base)
        }
    }
}

fn render_add(
    deck: &WorkspaceDeck,
    add: &AddWorkspace,
    height: usize,
    width: usize,
    base: &[String],
) -> Vec<String> {
    let inner = modal::modal_inner_width(width, 60);
    let visible = add.visible();
    let rows = visible
        .iter()
        .enumerate()
        .map(|(index, workspace)| {
            let marker = modal::selection_marker(index == add.cursor);
            let opened = add.opened.contains(&workspace.path);
            let checked = opened || add.selected.contains(&workspace.path);
            let row = format!(
                "  {marker} [{}] {}",
                if opened {
                    '✓'
                } else if checked {
                    'x'
                } else {
                    ' '
                },
                workspace.name
            );
            if opened {
                Style::new().dim().paint(&row)
            } else if index == add.cursor {
                Role::Accent.style().bold().paint(&row)
            } else {
                row
            }
        })
        .collect::<Vec<_>>();
    let mut body = vec![modal::filter_line(&add.filter, add.filter.len(), None)];
    let (start, end) = modal::list_window(rows.len(), add.cursor, 8);
    body.extend(modal::scroll_window(&rows, start, end));
    if let Some(notice) = deck.notice() {
        body.push(modal::error_line(notice, inner));
    }
    body.push(modal::footer(
        "type filter / Space select / Enter add / Esc cancel",
    ));
    modal::render_body_over(height, width, base, "Add workspace", inner, 13, body)
}

fn render_switcher(
    deck: &WorkspaceDeck,
    switcher: &ProjectSwitcher,
    height: usize,
    width: usize,
    base: &[String],
) -> Vec<String> {
    let inner = modal::modal_inner_width(width, 56);
    let rows = deck
        .slots
        .iter()
        .enumerate()
        .map(|(index, slot)| {
            let marker = modal::selection_marker(index == switcher.selected);
            let active = if slot.workspace_id == deck.active {
                "  active"
            } else {
                ""
            };
            let row = format!("  {marker} {}  {}{active}", index + 1, slot.label);
            if index == switcher.selected {
                Role::Accent.style().bold().paint(&row)
            } else {
                row
            }
        })
        .collect::<Vec<_>>();
    let (start, end) = modal::list_window(rows.len(), switcher.selected, 10);
    let mut body = modal::scroll_window(&rows, start, end);
    if let Some(notice) = deck.notice() {
        body.push(modal::error_line(notice, inner));
    }
    body.push(modal::footer(
        "↑↓ / 1..9 / Enter switch / x close / Esc cancel",
    ));
    modal::render_body_over(height, width, base, "Projects", inner, 13, body)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use usagi_core::domain::workspace_state::WorkspaceState;

    use super::*;

    fn snapshot(name: &str, path: &str) -> WorkspaceSnapshot {
        WorkspaceSnapshot::new(
            Workspace {
                name: name.to_owned(),
                path: PathBuf::from(path),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            },
            WorkspaceState::default(),
        )
    }

    #[test]
    fn deck_deduplicates_paths_and_commits_activation_by_identity() {
        let alpha = snapshot("alpha", "/alpha");
        let beta = snapshot("beta", "/beta");
        let duplicate = snapshot("alpha again", "/alpha");
        let mut deck = WorkspaceDeck::new(&alpha);
        deck.append_snapshots(&[beta.clone(), duplicate]);
        assert_eq!(
            deck.paths(),
            vec![PathBuf::from("/alpha"), PathBuf::from("/beta")]
        );
        deck.activate_snapshot(&beta);
        assert_eq!(deck.active_path(), Path::new("/beta"));
        assert_eq!(deck.slots()[1].workspace_id(), beta.workspace_id);
        assert_eq!(deck.path_at(1), Some(Path::new("/beta")));
        assert_eq!(deck.path_at(2), None);
        assert_eq!(WorkspaceDeck::from_snapshots(&[]), None);

        let absent = snapshot("absent", "/absent");
        deck.activate_snapshot(&absent);
        assert_eq!(deck.active_path(), Path::new("/beta"));
    }

    #[test]
    fn close_falls_right_then_left_and_last_has_no_replacement() {
        let alpha = snapshot("alpha", "/alpha");
        let beta = snapshot("beta", "/beta");
        let gamma = snapshot("gamma", "/gamma");
        let mut deck = WorkspaceDeck::from_snapshots(&[alpha, beta.clone(), gamma]).unwrap();
        deck.activate_snapshot(&beta);
        assert_eq!(
            deck.replacement_path_after_close(Path::new("/beta")),
            Some(Path::new("/gamma"))
        );
        deck.close_path(Path::new("/beta"));
        assert_eq!(deck.active_path(), Path::new("/gamma"));
        assert_eq!(
            deck.replacement_path_after_close(Path::new("/gamma")),
            Some(Path::new("/alpha"))
        );
        deck.close_path(Path::new("/gamma"));
        assert_eq!(deck.active_path(), Path::new("/alpha"));
        assert_eq!(deck.replacement_path_after_close(Path::new("/alpha")), None);
        deck.close_path(Path::new("/missing"));
    }

    #[test]
    fn add_overlay_keeps_registry_order_and_disables_open_paths() {
        let alpha = snapshot("alpha", "/alpha");
        let beta = Workspace::new("beta", "/beta");
        let gamma = Workspace::new("gamma", "/gamma");
        let mut deck = WorkspaceDeck::new(&alpha);
        deck.open_add(&[alpha.workspace.clone(), beta, gamma]);
        assert_eq!(
            deck.handle_overlay_key(&Key::Char(' ')),
            OverlayIntent::Stay
        );
        assert_eq!(deck.handle_overlay_key(&Key::Down), OverlayIntent::Stay);
        assert_eq!(
            deck.handle_overlay_key(&Key::Char(' ')),
            OverlayIntent::Stay
        );
        assert_eq!(deck.handle_overlay_key(&Key::Down), OverlayIntent::Stay);
        assert_eq!(
            deck.handle_overlay_key(&Key::Char(' ')),
            OverlayIntent::Stay
        );
        assert_eq!(
            deck.handle_overlay_key(&Key::Enter),
            OverlayIntent::Add(vec![PathBuf::from("/beta"), PathBuf::from("/gamma")])
        );
    }

    #[test]
    fn add_overlay_filters_edits_and_cancels_without_mutating_the_deck() {
        let alpha = snapshot("alpha", "/alpha");
        let beta = Workspace::new("beta", "/beta");
        let mut deck = WorkspaceDeck::new(&alpha);
        assert_eq!(deck.handle_overlay_key(&Key::Enter), OverlayIntent::Stay);

        deck.open_add(&[alpha.workspace.clone(), beta]);
        assert_eq!(
            deck.handle_overlay_key(&Key::Char('b')),
            OverlayIntent::Stay
        );
        assert_eq!(
            deck.handle_overlay_key(&Key::Paste("et".to_owned())),
            OverlayIntent::Stay
        );
        assert_eq!(
            deck.handle_overlay_key(&Key::Backspace),
            OverlayIntent::Stay
        );
        assert_eq!(deck.handle_overlay_key(&Key::Up), OverlayIntent::Stay);
        assert_eq!(deck.handle_overlay_key(&Key::Tab), OverlayIntent::Stay);
        assert_eq!(deck.handle_overlay_key(&Key::Escape), OverlayIntent::Cancel);
        assert_eq!(deck.paths(), vec![PathBuf::from("/alpha")]);
    }

    #[test]
    fn switcher_reaches_tenth_tab_and_closes_selected_identity() {
        let snapshots = (0..10)
            .map(|index| snapshot(&format!("project-{index}"), &format!("/project-{index}")))
            .collect::<Vec<_>>();
        let mut deck = WorkspaceDeck::from_snapshots(&snapshots).unwrap();
        deck.open_switcher();
        for _ in 0..9 {
            assert_eq!(deck.handle_overlay_key(&Key::Down), OverlayIntent::Stay);
        }
        assert_eq!(
            deck.handle_overlay_key(&Key::Enter),
            OverlayIntent::Activate(PathBuf::from("/project-9"))
        );
        assert_eq!(
            deck.handle_overlay_key(&Key::Char('x')),
            OverlayIntent::Close(PathBuf::from("/project-9"))
        );
    }

    #[test]
    fn switcher_supports_direct_digits_navigation_and_cancel() {
        let alpha = snapshot("alpha", "/alpha");
        let beta = snapshot("beta", "/beta");
        let mut deck = WorkspaceDeck::from_snapshots(&[alpha, beta]).unwrap();
        deck.open_switcher();
        assert_eq!(deck.handle_overlay_key(&Key::Up), OverlayIntent::Stay);
        assert_eq!(
            deck.handle_overlay_key(&Key::Char('2')),
            OverlayIntent::Activate(PathBuf::from("/beta"))
        );
        assert_eq!(
            deck.handle_overlay_key(&Key::Char('9')),
            OverlayIntent::Stay
        );
        assert_eq!(deck.handle_overlay_key(&Key::Tab), OverlayIntent::Stay);
        assert_eq!(deck.handle_overlay_key(&Key::Escape), OverlayIntent::Cancel);

        let mut empty = ProjectSwitcher { selected: 0 };
        assert_eq!(empty.handle(&Key::Enter, &[]), OverlayIntent::Stay);
        assert_eq!(empty.handle(&Key::Char('x'), &[]), OverlayIntent::Stay);
    }

    #[test]
    fn narrow_cjk_bar_keeps_active_identity_visible_and_hits_exact_path() {
        let alpha = snapshot("長い兎プロジェクト", "/alpha");
        let beta = snapshot("別の長い名前", "/beta");
        let mut deck = WorkspaceDeck::from_snapshots(&[alpha, beta.clone()]).unwrap();
        deck.activate_snapshot(&beta);
        let bar = project_bar(&deck, 24);
        assert!(widgets::display_width(&bar.line) <= 24);
        let hit = bar
            .hits
            .iter()
            .find(|hit| matches!(&hit.target, ProjectBarTarget::Workspace(path) if path == Path::new("/beta")))
            .unwrap();
        assert_eq!(bar.target_at(hit.columns.start), Some(&hit.target));
    }

    #[test]
    fn project_bar_handles_empty_wide_and_both_overflow_directions() {
        let alpha = snapshot("alpha", "/alpha");
        let snapshots = (0..10)
            .map(|index| snapshot(&format!("project-{index}"), &format!("/project-{index}")))
            .collect::<Vec<_>>();
        let mut empty = WorkspaceDeck::new(&alpha);
        empty.close_path(Path::new("/alpha"));
        assert_eq!(project_bar(&empty, 80).line, "");
        assert_eq!(project_bar(&WorkspaceDeck::new(&alpha), 0).line, "");

        let wide = project_bar(&WorkspaceDeck::from_snapshots(&snapshots[..3]).unwrap(), 80);
        assert!(wide.line.contains("project-0"));
        assert!(wide.line.contains("project-1"));
        assert!(
            wide.hits
                .iter()
                .any(|hit| hit.target == ProjectBarTarget::Add)
        );

        let left = WorkspaceDeck::from_snapshots(&snapshots).unwrap();
        let right_overflow = project_bar(&left, 50);
        assert!(right_overflow.line.contains("… +"));

        let mut right = left;
        right.activate_snapshot(&snapshots[9]);
        let left_overflow = project_bar(&right, 50);
        assert!(left_overflow.line.contains("… +"));
    }

    #[test]
    fn overlay_rendering_preserves_frame_size() {
        let alpha = snapshot("alpha", "/alpha");
        let beta = snapshot("beta", "/beta");
        let mut deck = WorkspaceDeck::from_snapshots(&[alpha, beta]).unwrap();
        deck.open_switcher();
        deck.set_notice("safe failure");
        let frame = render_overlay(&deck, 20, 80, &vec![String::new(); 20]);
        assert_eq!(frame.len(), 20);
        assert!(frame.iter().any(|line| line.contains("Projects")));
        deck.close_overlay();
        assert!(!deck.overlay_open());
        assert_eq!(render_overlay(&deck, 20, 80, &frame), frame);
    }
}

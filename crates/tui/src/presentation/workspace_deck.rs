//! Process-level project tabs shown above one active workspace Home.
//!
//! The deck owns workspace membership, ordering, active identity, its two small
//! overlays, and a read-only session snapshot for inactive Garden plots.
//! Mutable session, pane, and Agent state remain owned by the active workspace
//! controller.

use std::collections::{BTreeMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

use usagi_core::domain::agent::{AgentStatus, AgentWorkspaceObservation};
use usagi_core::domain::id::{SessionId, WorkspaceId};
use usagi_core::domain::session_lifecycle::SessionLifecycle;
use usagi_core::domain::workspace::Workspace;

use crate::presentation::theme::{Role, Style};
use crate::presentation::views::workspace::{ProjectedSession, present_agent_phase};
use crate::presentation::widgets::button::InlineButton;
use crate::presentation::widgets::garden::{GardenAgent, GardenSession};
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
    sessions: Vec<CachedGardenSession>,
    /// Whether the Garden's cross-project observation lane has seen this
    /// project's daemon Agent inventory since the cache was last rebuilt. An
    /// unobserved slot keeps drawing the read-only `project inactive` plot
    /// instead of claiming that an empty cached list means "no Agents".
    agents_observed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedGardenSession {
    projected: ProjectedSession,
    /// Agent runtimes the observation lane last saw in this session. They are
    /// membership only: the lane observes the daemon inventory, never this
    /// project's controller, so the phases stay the coarse inventory states.
    agents: Vec<GardenAgent>,
    /// Daemon-owned dispatch status observed in the same response as runtime
    /// membership. `None` means this session has no dispatch Agent.
    agent_status: Option<AgentStatus>,
}

impl CachedGardenSession {
    fn from_projected(session: &ProjectedSession) -> Self {
        Self {
            projected: session.clone(),
            agents: Vec::new(),
            agent_status: None,
        }
    }

    /// Project one cached session, carrying the observed Agents when the lane
    /// has seen this project.
    ///
    /// `observed` covers Agent membership only. A cached lifecycle that is not
    /// `Available` describes a transition this deck stopped watching when the
    /// tab went inactive, so it keeps the still read-only plot rather than
    /// being animated as if it were live.
    fn garden_session(&self, observed: bool) -> GardenSession {
        let observed = observed && self.projected.lifecycle == SessionLifecycle::Available;
        GardenSession {
            id: self.projected.id,
            label: self.projected.label.clone(),
            lifecycle: self.projected.lifecycle,
            selected: false,
            failure_summary: self.projected.failure_summary.clone(),
            agents_observed: observed,
            agents: if observed {
                self.agents.clone()
            } else {
                Vec::new()
            },
            agent_status: observed.then_some(self.agent_status).flatten(),
            pr_merged: false,
        }
    }
}

impl WorkspaceSlot {
    /// Project a freshly attached snapshot into deck metadata.
    #[must_use]
    pub fn from_snapshot(snapshot: &WorkspaceSnapshot) -> Self {
        let sessions = snapshot
            .state
            .sessions
            .iter()
            .zip(&snapshot.session_ids)
            .map(|(record, id)| {
                let projection = snapshot.session_lifecycles.get(id);
                let mut projected = ProjectedSession::from_record(*id, record);
                if let Some(projection) = projection {
                    projected.lifecycle = projection.lifecycle;
                    projected
                        .failure_stage
                        .clone_from(&projection.failure_stage);
                    projected
                        .failure_summary
                        .clone_from(&projection.failure_summary);
                }
                CachedGardenSession {
                    projected,
                    agents: Vec::new(),
                    agent_status: None,
                }
            })
            .collect();
        Self {
            path: snapshot.workspace.path.clone(),
            workspace_id: snapshot.workspace_id,
            label: snapshot.workspace.name.clone(),
            sessions,
            agents_observed: false,
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

    /// Cached session rows available without reopening this workspace.
    #[must_use]
    pub fn projected_sessions(&self) -> Vec<ProjectedSession> {
        self.sessions
            .iter()
            .map(|session| session.projected.clone())
            .collect()
    }
}

/// Which process-level modal is in front of Home.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DeckOverlay {
    Add(AddWorkspace),
    Switcher(ProjectSwitcher),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddWorkspaceMode {
    Registered,
    Directory,
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
    pending_garden_visit: Option<(PathBuf, SessionId)>,
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
            pending_garden_visit: None,
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

    /// Project tab immediately before the active one, wrapping at the start.
    #[must_use]
    pub fn previous_path(&self) -> &Path {
        let index = self
            .active_index()
            .checked_sub(1)
            .unwrap_or(self.slots.len() - 1);
        self.slots[index].path()
    }

    /// Project tab immediately after the active one, wrapping at the end.
    #[must_use]
    pub fn next_path(&self) -> &Path {
        let index = (self.active_index() + 1) % self.slots.len();
        self.slots[index].path()
    }

    #[must_use]
    pub fn path_at(&self, index: usize) -> Option<&Path> {
        self.slots.get(index).map(WorkspaceSlot::path)
    }

    #[must_use]
    pub fn path_for_workspace(&self, workspace: WorkspaceId) -> Option<&Path> {
        self.slots
            .iter()
            .find(|slot| slot.workspace_id == workspace)
            .map(WorkspaceSlot::path)
    }

    #[must_use]
    pub fn contains_path(&self, path: &Path) -> bool {
        self.slots.iter().any(|slot| slot.path == path)
    }

    /// Read the already-open slot for a transition frame without daemon IO.
    #[must_use]
    pub fn slot_for_path(&self, path: &Path) -> Option<&WorkspaceSlot> {
        self.slots.iter().find(|slot| slot.path == path)
    }

    /// Select an existing slot in a presentation-only deck clone.
    pub fn preview_path(&mut self, path: &Path) {
        if let Some(slot) = self.slots.iter().find(|slot| slot.path == path) {
            self.active = slot.workspace_id;
            self.overlay = None;
            self.notice = None;
        }
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
            slot.sessions = WorkspaceSlot::from_snapshot(snapshot).sessions;
            slot.agents_observed = false;
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

    /// Replace the candidates of an already-open add overlay with a fresh
    /// global registry snapshot. Filter text, directory input, and selections
    /// survive so another usagi process can update the list without disrupting
    /// the current interaction.
    pub fn refresh_add(&mut self, registry: &[Workspace]) {
        if let Some(DeckOverlay::Add(add)) = self.overlay.as_mut() {
            add.refresh(registry);
        }
    }

    #[must_use]
    pub fn add_overlay_open(&self) -> bool {
        matches!(self.overlay, Some(DeckOverlay::Add(_)))
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
        if self
            .pending_garden_visit
            .as_ref()
            .is_some_and(|(target, _)| target == path)
        {
            self.pending_garden_visit = None;
        }
    }

    /// Carry one identity-only Garden visit across the workspace composition
    /// teardown. It is consumed only by the prepared target workspace.
    pub fn schedule_garden_visit(&mut self, path: PathBuf, session: SessionId) {
        self.pending_garden_visit = Some((path, session));
    }

    #[must_use]
    pub fn take_garden_visit(&mut self, active_path: &Path) -> Option<SessionId> {
        if self
            .pending_garden_visit
            .as_ref()
            .is_some_and(|(path, _)| path == active_path)
        {
            return self.pending_garden_visit.take().map(|(_, session)| session);
        }
        None
    }

    /// Keep the active tab's inactive-Garden fallback current without retaining
    /// its workspace controller or Agent inventory.
    pub fn update_active_sessions(&mut self, sessions: &[ProjectedSession]) {
        if let Some(slot) = self
            .slots
            .iter_mut()
            .find(|slot| slot.workspace_id == self.active)
        {
            slot.sessions = sessions
                .iter()
                .map(CachedGardenSession::from_projected)
                .collect();
            // The active project draws from its controller, so this cache is
            // only what the plot falls back to once the tab goes inactive. It
            // describes no observation until the lane makes one.
            slot.agents_observed = false;
        }
    }

    /// Every open project the Garden observes from the outside: the tabs whose
    /// workspace controller is not resident in this process.
    ///
    /// The active project is excluded because its own controller already
    /// projects richer, runtime-local Agent phases into the Garden.
    #[must_use]
    pub fn observable_workspaces(&self) -> Vec<WorkspaceId> {
        self.slots
            .iter()
            .map(WorkspaceSlot::workspace_id)
            .filter(|workspace| *workspace != self.active)
            .collect()
    }

    /// Attach one inactive project's daemon Agent inventory to its cached plots.
    ///
    /// The inventory is membership authority exactly as it is for the active
    /// project's `HomeProjection::with_agent_inventory`: a runtime it does not
    /// hold in a present state draws no rabbit, workspace-root runtimes belong
    /// to no plot, and a runtime naming a session this cache does not know is
    /// dropped rather than inventing a plot for it.
    ///
    /// Returns whether the projection changed, so the shell can redraw a Garden
    /// whose other material (session list, animation tick) stood still.
    pub fn apply_garden_inventory(&mut self, observation: &AgentWorkspaceObservation) -> bool {
        let inventory = &observation.inventory;
        if inventory.workspace_id == self.active {
            return false;
        }
        let Some(slot) = self
            .slots
            .iter_mut()
            .find(|slot| slot.workspace_id == inventory.workspace_id)
        else {
            return false;
        };
        let mut observed: BTreeMap<SessionId, Vec<GardenAgent>> = BTreeMap::new();
        for item in &inventory.runtimes {
            let Some(session_id) = item.runtime.session_id else {
                continue;
            };
            let Some(phase) = present_agent_phase(item.state) else {
                continue;
            };
            observed.entry(session_id).or_default().push(GardenAgent {
                runtime_id: item.runtime.agent_runtime_id,
                phase,
            });
        }
        let mut changed = !slot.agents_observed;
        slot.agents_observed = true;
        for session in &mut slot.sessions {
            let agents = observed.remove(&session.projected.id).unwrap_or_default();
            if session.agents != agents {
                session.agents = agents;
                changed = true;
            }
            let agent_status = observation
                .session_statuses
                .get(&session.projected.id)
                .copied();
            if session.agent_status != agent_status {
                session.agent_status = agent_status;
                changed = true;
            }
        }
        changed
    }

    /// Flatten every open project's cached sessions for the process-level
    /// Garden. The active project keeps its richer, current Agent projection;
    /// inactive projects remain read-only cached plots until visited.
    #[must_use]
    pub fn garden_projection(
        &self,
        active_sessions: &[GardenSession],
    ) -> (String, Vec<(WorkspaceId, GardenSession)>) {
        let multiple_projects = self.slots.len() > 1;
        let scope = if multiple_projects {
            format!("{} open projects", self.slots.len())
        } else {
            self.slots
                .first()
                .map_or_else(String::new, |slot| slot.label.clone())
        };
        let mut projected = Vec::new();
        for slot in &self.slots {
            if slot.workspace_id == self.active {
                projected.extend(active_sessions.iter().cloned().map(|session| {
                    (
                        slot.workspace_id,
                        project_labeled(session, slot, multiple_projects),
                    )
                }));
            } else {
                projected.extend(slot.sessions.iter().map(|session| {
                    (
                        slot.workspace_id,
                        project_labeled(
                            session.garden_session(slot.agents_observed),
                            slot,
                            multiple_projects,
                        ),
                    )
                }));
            }
        }
        (scope, projected)
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

fn project_labeled(
    mut session: GardenSession,
    slot: &WorkspaceSlot,
    multiple_projects: bool,
) -> GardenSession {
    if multiple_projects {
        session.label = format!("{} / {}", slot.label, session.label);
    }
    session
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AddWorkspace {
    candidates: Vec<Workspace>,
    opened: HashSet<PathBuf>,
    selected: HashSet<PathBuf>,
    cursor: usize,
    filter: String,
    directory: String,
    mode: AddWorkspaceMode,
}

impl AddWorkspace {
    fn new(registry: &[Workspace], opened: HashSet<PathBuf>) -> Self {
        Self {
            candidates: registry.to_vec(),
            opened,
            selected: HashSet::new(),
            cursor: 0,
            filter: String::new(),
            directory: String::new(),
            mode: AddWorkspaceMode::Registered,
        }
    }

    fn refresh(&mut self, registry: &[Workspace]) {
        let cursor_path = self
            .visible()
            .get(self.cursor)
            .map(|workspace| workspace.path.clone());
        self.candidates = registry.to_vec();
        let candidates = self
            .candidates
            .iter()
            .map(|workspace| workspace.path.clone())
            .collect::<HashSet<_>>();
        self.selected
            .retain(|path| candidates.contains(path) && !self.opened.contains(path));
        let visible = self.visible();
        self.cursor = cursor_path
            .and_then(|path| visible.iter().position(|workspace| workspace.path == path))
            .unwrap_or_else(|| self.cursor.min(visible.len().saturating_sub(1)));
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
        if matches!(key, Key::Tab) {
            self.mode = match self.mode {
                AddWorkspaceMode::Registered => AddWorkspaceMode::Directory,
                AddWorkspaceMode::Directory => AddWorkspaceMode::Registered,
            };
            return OverlayIntent::Stay;
        }
        if self.mode == AddWorkspaceMode::Directory {
            return self.handle_directory(key);
        }
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
            Key::CtrlD => {
                let path = self
                    .visible()
                    .get(self.cursor)
                    .map(|workspace| workspace.path.clone());
                if let Some(path) = path.filter(|path| self.opened.contains(path)) {
                    return OverlayIntent::Close(path);
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

    fn handle_directory(&mut self, key: &Key) -> OverlayIntent {
        match key {
            Key::Backspace => {
                self.directory.pop();
            }
            Key::Char(character) => self.directory.push(*character),
            Key::Paste(text) => self.directory.push_str(text),
            Key::Enter => {
                let directory = self.directory.trim();
                if !directory.is_empty() {
                    return OverlayIntent::Add(vec![PathBuf::from(directory)]);
                }
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
    let add_button = InlineButton::new(ADD_LABEL);
    let add_width = add_button.width();

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
        let add = add_button.render(
            width.saturating_sub(column),
            Role::Accent.style().bold().reverse(),
        );
        let span = add.width;
        line.push_str(&add.line);
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
    if add.mode == AddWorkspaceMode::Directory {
        let mut body = vec![
            Style::new().dim().paint("  Directory"),
            modal::filter_line(&add.directory, add.directory.len(), None),
            String::new(),
            Style::new()
                .dim()
                .paint("  Enter an existing directory to register and open it."),
        ];
        if let Some(notice) = deck.notice() {
            body.push(modal::error_line(notice, inner));
        }
        body.push(modal::footer("Enter add / Tab registered / Esc cancel"));
        return modal::render_body_over(height, width, base, "Add workspace", inner, 13, body);
    }
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
        "type filter / Space select / Tab directory / Ctrl-D close / Enter add / Esc",
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
    use usagi_core::domain::note::Scratchpad;
    use usagi_core::domain::session::{SessionOrigin, SessionRecord};
    use usagi_core::domain::session_lifecycle::AgentPhase;
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

    fn snapshot_with_session(name: &str, path: &str, session: &str) -> WorkspaceSnapshot {
        let state = WorkspaceState {
            sessions: vec![SessionRecord {
                name: session.to_owned(),
                display_name: None,
                origin: SessionOrigin::Human,
                started_from: None,
                root: PathBuf::from(path).join(session),
                created_at: Utc::now(),
                last_active: None,
                notes: Scratchpad::default(),
                prs: Vec::new(),
            }],
            ..WorkspaceState::default()
        };
        WorkspaceSnapshot::new(
            Workspace {
                name: name.to_owned(),
                path: PathBuf::from(path),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            },
            state,
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
    fn adjacent_paths_follow_deck_order_and_wrap() {
        let alpha = snapshot("alpha", "/alpha");
        let beta = snapshot("beta", "/beta");
        let gamma = snapshot("gamma", "/gamma");
        let mut deck =
            WorkspaceDeck::from_snapshots(&[alpha.clone(), beta.clone(), gamma.clone()]).unwrap();

        assert_eq!(deck.previous_path(), Path::new("/gamma"));
        assert_eq!(deck.next_path(), Path::new("/beta"));

        deck.activate_snapshot(&gamma);
        assert_eq!(deck.previous_path(), Path::new("/beta"));
        assert_eq!(deck.next_path(), Path::new("/alpha"));

        let single = WorkspaceDeck::new(&alpha);
        assert_eq!(single.previous_path(), Path::new("/alpha"));
        assert_eq!(single.next_path(), Path::new("/alpha"));
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
        assert_eq!(deck.handle_overlay_key(&Key::Other), OverlayIntent::Stay);
        assert_eq!(deck.handle_overlay_key(&Key::Tab), OverlayIntent::Stay);
        assert_eq!(deck.handle_overlay_key(&Key::Escape), OverlayIntent::Cancel);
        assert_eq!(deck.paths(), vec![PathBuf::from("/alpha")]);
    }

    #[test]
    fn add_overlay_accepts_an_existing_directory_path() {
        let alpha = snapshot("alpha", "/alpha");
        let mut deck = WorkspaceDeck::new(&alpha);
        deck.open_add(std::slice::from_ref(&alpha.workspace));

        assert_eq!(deck.handle_overlay_key(&Key::Tab), OverlayIntent::Stay);
        assert_eq!(deck.handle_overlay_key(&Key::Enter), OverlayIntent::Stay);
        assert_eq!(
            deck.handle_overlay_key(&Key::Char('x')),
            OverlayIntent::Stay
        );
        assert_eq!(
            deck.handle_overlay_key(&Key::Backspace),
            OverlayIntent::Stay
        );
        assert_eq!(deck.handle_overlay_key(&Key::Down), OverlayIntent::Stay);
        assert_eq!(deck.handle_overlay_key(&Key::Tab), OverlayIntent::Stay);
        assert_eq!(deck.handle_overlay_key(&Key::Tab), OverlayIntent::Stay);
        assert_eq!(
            deck.handle_overlay_key(&Key::Paste(" /projects/new workspace ".to_owned())),
            OverlayIntent::Stay
        );
        assert_eq!(
            deck.handle_overlay_key(&Key::Enter),
            OverlayIntent::Add(vec![PathBuf::from("/projects/new workspace")])
        );
    }

    #[test]
    fn add_overlay_refreshes_candidates_without_losing_the_current_interaction() {
        let alpha = snapshot("alpha", "/alpha");
        let beta = Workspace::new("beta", "/beta");
        let gamma = Workspace::new("gamma", "/gamma");
        let mut deck = WorkspaceDeck::new(&alpha);
        deck.open_add(&[alpha.workspace.clone(), beta.clone()]);
        assert!(deck.add_overlay_open());
        let _ = deck.handle_overlay_key(&Key::Down);
        let _ = deck.handle_overlay_key(&Key::Char(' '));

        deck.refresh_add(&[alpha.workspace.clone(), beta, gamma]);
        let _ = deck.handle_overlay_key(&Key::Down);
        let _ = deck.handle_overlay_key(&Key::Char(' '));
        assert_eq!(
            deck.handle_overlay_key(&Key::Enter),
            OverlayIntent::Add(vec![PathBuf::from("/beta"), PathBuf::from("/gamma")])
        );

        deck.refresh_add(std::slice::from_ref(&alpha.workspace));
        assert_eq!(
            deck.handle_overlay_key(&Key::Enter),
            OverlayIntent::Add(Vec::new())
        );

        deck.close_overlay();
        deck.refresh_add(&[]);
        assert!(!deck.add_overlay_open());
    }

    #[test]
    fn add_overlay_closes_only_the_open_workspace_under_its_cursor() {
        let alpha = snapshot("alpha", "/alpha");
        let beta = Workspace::new("beta", "/beta");
        let mut deck = WorkspaceDeck::new(&alpha);
        deck.open_add(&[alpha.workspace.clone(), beta]);

        assert_eq!(
            deck.handle_overlay_key(&Key::CtrlD),
            OverlayIntent::Close(PathBuf::from("/alpha"))
        );
        assert_eq!(deck.handle_overlay_key(&Key::Down), OverlayIntent::Stay);
        assert_eq!(deck.handle_overlay_key(&Key::CtrlD), OverlayIntent::Stay);

        // Plain `x` remains filter text; close does not steal a searchable name.
        assert_eq!(
            deck.handle_overlay_key(&Key::Char('x')),
            OverlayIntent::Stay
        );
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
    fn add_button_clicks_include_plus_and_both_padding_cells() {
        let deck = WorkspaceDeck::new(&snapshot("alpha", "/alpha"));
        let bar = project_bar(&deck, 80);
        let hit = bar
            .hits
            .iter()
            .find(|hit| hit.target == ProjectBarTarget::Add)
            .expect("the add button is visible");
        assert_eq!(hit.columns.len(), InlineButton::new(ADD_LABEL).width());
        for column in [
            hit.columns.start,
            hit.columns.start + 1,
            hit.columns.end - 1,
        ] {
            assert_eq!(bar.target_at(column), Some(&ProjectBarTarget::Add));
        }
    }

    #[test]
    fn garden_flattens_every_open_project_and_carries_a_visit_across_activation() {
        let alpha = snapshot_with_session("alpha", "/alpha", "build");
        let beta = snapshot_with_session("beta", "/beta", "review");
        let mut deck = WorkspaceDeck::from_snapshots(&[alpha.clone(), beta.clone()]).unwrap();
        assert_eq!(
            deck.path_for_workspace(beta.workspace_id),
            Some(beta.workspace.path.as_path())
        );
        assert_eq!(deck.path_for_workspace(WorkspaceId::new()), None);
        let active = vec![GardenSession {
            id: alpha.session_ids[0],
            label: "◆ Manager · build".to_owned(),
            lifecycle: SessionLifecycle::Available,
            selected: true,
            failure_summary: None,
            agents_observed: true,
            agents: Vec::new(),
            agent_status: None,
            pr_merged: false,
        }];

        let (scope, sessions) = deck.garden_projection(&active);
        assert_eq!(scope, "2 open projects");
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].0, alpha.workspace_id);
        assert_eq!(sessions[0].1.label, "alpha / ◆ Manager · build");
        assert_eq!(sessions[1].0, beta.workspace_id);
        assert_eq!(sessions[1].1.label, "beta / review");

        deck.schedule_garden_visit(beta.workspace.path.clone(), beta.session_ids[0]);
        assert_eq!(deck.take_garden_visit(&alpha.workspace.path), None);
        assert_eq!(
            deck.take_garden_visit(&beta.workspace.path),
            Some(beta.session_ids[0])
        );
        assert_eq!(deck.take_garden_visit(&beta.workspace.path), None);
    }

    #[test]
    fn garden_cache_tracks_active_rows_and_single_project_labels_stay_compact() {
        let alpha = snapshot_with_session("alpha", "/alpha", "old");
        let beta = snapshot_with_session("beta", "/beta", "review");
        let mut deck = WorkspaceDeck::from_snapshots(&[alpha.clone(), beta.clone()]).unwrap();
        let fresh = ProjectedSession::from_record(
            alpha.session_ids[0],
            &SessionRecord {
                name: "fresh".to_owned(),
                display_name: None,
                origin: SessionOrigin::Human,
                started_from: None,
                root: PathBuf::from("/alpha/fresh"),
                created_at: Utc::now(),
                last_active: None,
                notes: Scratchpad::default(),
                prs: Vec::new(),
            },
        );
        deck.update_active_sessions(std::slice::from_ref(&fresh));
        deck.activate_snapshot(&beta);
        let active_beta = deck.slots[1].sessions[0].garden_session(false);
        let (_, sessions) = deck.garden_projection(std::slice::from_ref(&active_beta));
        assert_eq!(sessions[0].1.label, "alpha / fresh");

        let single = WorkspaceDeck::new(&alpha);
        let active_alpha = single.slots[0].sessions[0].garden_session(false);
        let (scope, sessions) = single.garden_projection(std::slice::from_ref(&active_alpha));
        assert_eq!(scope, "alpha");
        assert_eq!(sessions[0].1.label, "old");

        let mut empty = single;
        empty.close_path(Path::new("/alpha"));
        empty.update_active_sessions(&[]);
        assert_eq!(empty.garden_projection(&[]), (String::new(), Vec::new()));
    }

    fn runtime_ref(
        workspace: WorkspaceId,
        session: Option<SessionId>,
    ) -> usagi_core::domain::id::AgentRuntimeRef {
        usagi_core::domain::id::AgentRuntimeRef {
            agent_runtime_id: usagi_core::domain::id::AgentRuntimeId::new(),
            terminal: usagi_core::domain::id::TerminalRef {
                daemon_generation: usagi_core::domain::id::DaemonGeneration::new(),
                terminal_id: usagi_core::domain::id::TerminalId::new(),
                workspace_id: workspace,
                session_id: session,
                worktree_id: usagi_core::domain::id::WorktreeId::new(),
            },
            session_id: session,
        }
    }

    /// The plot the deck projects for one open project, of which these
    /// fixtures give every project exactly one.
    fn plot_of(deck: &WorkspaceDeck, workspace: WorkspaceId) -> GardenSession {
        deck.garden_projection(&[])
            .1
            .into_iter()
            .find(|(id, _)| *id == workspace)
            .expect("every open project owns a plot")
            .1
    }

    fn inventory(
        workspace: WorkspaceId,
        runtimes: Vec<(
            usagi_core::domain::id::AgentRuntimeRef,
            usagi_core::domain::agent::AgentRuntimeInventoryState,
        )>,
    ) -> AgentWorkspaceObservation {
        AgentWorkspaceObservation {
            inventory: usagi_core::domain::agent::AgentInventory {
                workspace_id: workspace,
                runtimes: runtimes
                    .into_iter()
                    .map(
                        |(runtime, state)| usagi_core::domain::agent::AgentRuntimeInventoryItem {
                            runtime,
                            continuation: usagi_core::domain::id::AgentContinuationRef::new(),
                            state,
                            resumed_from: None,
                        },
                    )
                    .collect(),
                resumable: Vec::new(),
            },
            session_statuses: BTreeMap::new(),
        }
    }

    fn inventory_with_status(
        workspace: WorkspaceId,
        runtimes: Vec<(
            usagi_core::domain::id::AgentRuntimeRef,
            usagi_core::domain::agent::AgentRuntimeInventoryState,
        )>,
        session: SessionId,
        status: AgentStatus,
    ) -> AgentWorkspaceObservation {
        let mut observation = inventory(workspace, runtimes);
        observation.session_statuses.insert(session, status);
        observation
    }

    /// The Garden draws every open project, so an inactive project's plots take
    /// their rabbits from the daemon inventory observed for *that* workspace.
    #[test]
    fn an_inactive_projects_plot_draws_the_agents_observed_for_its_own_workspace() {
        use usagi_core::domain::agent::AgentRuntimeInventoryState;

        let alpha = snapshot_with_session("alpha", "/alpha", "build");
        let beta = snapshot_with_session("beta", "/beta", "review");
        let mut deck = WorkspaceDeck::from_snapshots(&[alpha.clone(), beta.clone()]).unwrap();
        assert_eq!(deck.observable_workspaces(), vec![beta.workspace_id]);

        // Before any observation the plot stays the read-only one: an empty
        // cached list is not evidence that the project owns no Agents.
        let plot = plot_of(&deck, beta.workspace_id);
        assert!(!plot.agents_observed);
        assert!(plot.agents.is_empty());

        let live = runtime_ref(beta.workspace_id, Some(beta.session_ids[0]));
        let closed = runtime_ref(beta.workspace_id, Some(beta.session_ids[0]));
        let root = runtime_ref(beta.workspace_id, None);
        let foreign = runtime_ref(beta.workspace_id, Some(SessionId::new()));
        assert!(deck.apply_garden_inventory(&inventory_with_status(
            beta.workspace_id,
            vec![
                (live.clone(), AgentRuntimeInventoryState::Live),
                (closed, AgentRuntimeInventoryState::Exited),
                (root, AgentRuntimeInventoryState::Live),
                (foreign, AgentRuntimeInventoryState::Live),
            ],
            beta.session_ids[0],
            AgentStatus::Idle,
        )));

        let plot = plot_of(&deck, beta.workspace_id);
        assert!(plot.agents_observed);
        assert_eq!(plot.agent_status, Some(AgentStatus::Idle));
        // Only the runtime that owns a tab in a known session becomes a rabbit.
        assert_eq!(
            plot.agents
                .iter()
                .map(|agent| (agent.runtime_id, agent.phase))
                .collect::<Vec<_>>(),
            vec![(live.agent_runtime_id, AgentPhase::Running)]
        );

        // Re-observing the same inventory changes no draw material.
        assert!(!deck.apply_garden_inventory(&inventory_with_status(
            beta.workspace_id,
            vec![(live.clone(), AgentRuntimeInventoryState::Live)],
            beta.session_ids[0],
            AgentStatus::Idle,
        )));
        // The Agent leaving is membership too: the rabbit goes with it.
        assert!(deck.apply_garden_inventory(&inventory(beta.workspace_id, Vec::new())));
        let plot = plot_of(&deck, beta.workspace_id);
        assert!(plot.agents_observed);
        assert!(plot.agents.is_empty());
    }

    /// The active project's own controller owns its plots, and a cached
    /// lifecycle that is not `Available` describes a transition this deck
    /// stopped watching — neither may be overwritten by an observation.
    #[test]
    fn garden_observation_skips_the_active_project_and_cached_transitions() {
        use usagi_core::domain::agent::AgentRuntimeInventoryState;

        let alpha = snapshot_with_session("alpha", "/alpha", "build");
        let beta = snapshot_with_session("beta", "/beta", "review");
        let mut deck = WorkspaceDeck::from_snapshots(&[alpha.clone(), beta.clone()]).unwrap();
        let active_runtime = runtime_ref(alpha.workspace_id, Some(alpha.session_ids[0]));
        assert!(!deck.apply_garden_inventory(&inventory(
            alpha.workspace_id,
            vec![(active_runtime, AgentRuntimeInventoryState::Live)],
        )));
        // A workspace no tab holds is not a plot of this deck either.
        assert!(!deck.apply_garden_inventory(&inventory(WorkspaceId::new(), Vec::new())));

        deck.slots[1].sessions[0].projected.lifecycle = SessionLifecycle::Creating;
        let creating = runtime_ref(beta.workspace_id, Some(beta.session_ids[0]));
        assert!(deck.apply_garden_inventory(&inventory(
            beta.workspace_id,
            vec![(creating, AgentRuntimeInventoryState::Live)],
        )));
        let plot = plot_of(&deck, beta.workspace_id);
        assert!(!plot.agents_observed);
        assert!(plot.agents.is_empty());
    }

    /// Rebuilding a slot's cache — the active tab's sessions changing, or a tab
    /// being activated — drops the observation it carried, so a plot never
    /// shows Agents that belong to a session list it no longer describes.
    #[test]
    fn rebuilding_a_slots_cache_forgets_its_garden_observation() {
        use usagi_core::domain::agent::AgentRuntimeInventoryState;

        let alpha = snapshot_with_session("alpha", "/alpha", "build");
        let beta = snapshot_with_session("beta", "/beta", "review");
        let mut deck = WorkspaceDeck::from_snapshots(&[alpha.clone(), beta.clone()]).unwrap();
        let runtime = runtime_ref(beta.workspace_id, Some(beta.session_ids[0]));
        assert!(deck.apply_garden_inventory(&inventory(
            beta.workspace_id,
            vec![(runtime, AgentRuntimeInventoryState::Live)],
        )));

        deck.activate_snapshot(&beta);
        assert_eq!(deck.observable_workspaces(), vec![alpha.workspace_id]);
        assert!(!deck.slots[1].agents_observed);

        // The now-active project's cache is a fallback for when its tab goes
        // inactive again; it claims no observation of its own.
        deck.update_active_sessions(&[ProjectedSession::from_record(
            beta.session_ids[0],
            &beta.state.sessions[0],
        )]);
        assert!(!deck.slots[1].agents_observed);
    }

    #[test]
    fn closing_a_pending_garden_projects_tab_discards_the_visit() {
        let alpha = snapshot("alpha", "/alpha");
        let beta = snapshot("beta", "/beta");
        let mut deck = WorkspaceDeck::from_snapshots(&[alpha, beta.clone()]).unwrap();
        let session = SessionId::new();
        deck.schedule_garden_visit(beta.workspace.path.clone(), session);
        deck.close_path(&beta.workspace.path);
        assert_eq!(deck.take_garden_visit(&beta.workspace.path), None);
    }

    #[test]
    fn overlay_rendering_preserves_frame_size() {
        let alpha = snapshot("alpha", "/alpha");
        let beta = snapshot("beta", "/beta");
        let mut deck = WorkspaceDeck::from_snapshots(&[alpha.clone(), beta]).unwrap();
        deck.open_switcher();
        deck.set_notice("safe failure");
        let frame = render_overlay(&deck, 20, 80, &vec![String::new(); 20]);
        assert_eq!(frame.len(), 20);
        assert!(frame.iter().any(|line| line.contains("Projects")));
        deck.open_add(std::slice::from_ref(&alpha.workspace));
        let add = render_overlay(&deck, 20, 80, &vec![String::new(); 20]);
        assert!(add.iter().any(|line| line.contains("Ctrl-D close")));
        let _ = deck.handle_overlay_key(&Key::Tab);
        deck.set_notice("directory does not exist");
        let directory = render_overlay(&deck, 20, 80, &vec![String::new(); 20]);
        assert!(directory.iter().any(|line| line.contains("Directory")));
        assert!(
            directory
                .iter()
                .any(|line| line.contains("directory does not exist"))
        );
        deck.close_overlay();
        assert!(!deck.overlay_open());
        assert_eq!(render_overlay(&deck, 20, 80, &frame), frame);
    }
}

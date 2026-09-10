//! Welcome / Open attach flow.

use usagi_core::domain::id::{SessionId, WorkspaceId};

use super::{AppState, Effect, Notice};

/// One selectable workspace in the entry surfaces.
///
/// `label` is presentation data only. [`WorkspaceId`] is the identity retained
/// from Welcome / Open through the attach request and into the Home snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryWorkspace {
    /// Stable workspace incarnation.
    pub id: WorkspaceId,
    /// Name rendered by Welcome or Open.
    pub label: String,
}

impl EntryWorkspace {
    /// Create a selectable workspace projection.
    #[must_use]
    pub fn new(id: WorkspaceId, label: impl Into<String>) -> Self {
        Self {
            id,
            label: label.into(),
        }
    }
}

/// The typed part of the first Home response needed to initialize its reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeSnapshot {
    /// The workspace that was attached.
    pub workspace: WorkspaceId,
    /// Session identities in the snapshot order.
    pub sessions: Vec<SessionId>,
}

impl HomeSnapshot {
    /// Create a Home snapshot projection.
    #[must_use]
    pub fn new(workspace: WorkspaceId, sessions: Vec<SessionId>) -> Self {
        Self {
            workspace,
            sessions,
        }
    }
}

/// The entry surface currently visible before or after an attach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryRoute {
    /// The Welcome menu and its Recent cards.
    Welcome,
    /// The complete registered-workspace list.
    Open,
    /// An attached Home controller.
    Home(Box<AppState>),
}

/// Entry reducer input. The terminal adapter maps concrete keys to this small
/// vocabulary, while tests can drive it without a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryEvent {
    /// Move from Welcome to Open.
    ShowOpen,
    /// Select one Single item in Open by its typed identity.
    OpenSingle(WorkspaceId),
    /// Select one Welcome Recent item by its typed identity.
    OpenRecent(WorkspaceId),
    /// Retry the most recent failed attach on the same visible entry surface.
    Retry,
    /// Return from Open to Welcome.
    Back,
    /// Completion for a previously issued attach request.
    AttachResult {
        /// Identity echoed by the backend.
        workspace: WorkspaceId,
        /// A successful typed Home snapshot or a safe in-screen error.
        result: Result<HomeSnapshot, Notice>,
    },
}

/// State for the Welcome → Open / Recent → Home entry flow.
///
/// `opening` is an identity fence. Only a completion for this exact workspace
/// can enter Home; all other (including late) backend results are ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryState {
    route: EntryRoute,
    workspaces: Vec<EntryWorkspace>,
    recents: Vec<WorkspaceId>,
    opening: Option<WorkspaceId>,
    failed: Option<WorkspaceId>,
    error: Option<Notice>,
}

impl EntryState {
    /// Create an entry flow at Welcome. Recent IDs may refer to an item absent
    /// from the current Open list; the backend remains authoritative for that
    /// stale registration and reports an in-screen error if it cannot attach.
    #[must_use]
    pub fn new(workspaces: Vec<EntryWorkspace>, recents: Vec<WorkspaceId>) -> Self {
        Self {
            route: EntryRoute::Welcome,
            workspaces,
            recents,
            opening: None,
            failed: None,
            error: None,
        }
    }

    /// The current entry route.
    #[must_use]
    pub const fn route(&self) -> &EntryRoute {
        &self.route
    }

    /// Registered Open Single choices.
    #[must_use]
    pub fn workspaces(&self) -> &[EntryWorkspace] {
        &self.workspaces
    }

    /// Recent typed identities displayed by Welcome.
    #[must_use]
    pub fn recents(&self) -> &[WorkspaceId] {
        &self.recents
    }

    /// The attach currently in flight, if any.
    #[must_use]
    pub const fn opening(&self) -> Option<WorkspaceId> {
        self.opening
    }

    /// The last attach error, suitable for rendering on the current entry screen.
    #[must_use]
    pub fn error(&self) -> Option<&Notice> {
        self.error.as_ref()
    }

    fn start_open(&mut self, workspace: WorkspaceId) -> Vec<Effect> {
        if self.opening.is_some() {
            return Vec::new();
        }
        self.opening = Some(workspace);
        self.failed = None;
        self.error = None;
        vec![Effect::AttachWorkspace { workspace }]
    }
}

/// Reduce one entry event and return any backend work it requests.
#[must_use]
pub fn update_entry(state: &mut EntryState, event: EntryEvent) -> Vec<Effect> {
    match event {
        EntryEvent::ShowOpen if matches!(state.route, EntryRoute::Welcome) => {
            state.route = EntryRoute::Open;
            state.error = None;
            Vec::new()
        }
        EntryEvent::OpenSingle(workspace)
            if matches!(state.route, EntryRoute::Open)
                && state
                    .workspaces
                    .iter()
                    .any(|candidate| candidate.id == workspace) =>
        {
            state.start_open(workspace)
        }
        EntryEvent::OpenRecent(workspace)
            if matches!(state.route, EntryRoute::Welcome) && state.recents.contains(&workspace) =>
        {
            state.start_open(workspace)
        }
        EntryEvent::Retry if state.opening.is_none() => state
            .failed
            .map_or_else(Vec::new, |id| state.start_open(id)),
        EntryEvent::Back if matches!(state.route, EntryRoute::Open) && state.opening.is_none() => {
            state.route = EntryRoute::Welcome;
            state.error = None;
            Vec::new()
        }
        EntryEvent::AttachResult { workspace, result } if state.opening == Some(workspace) => {
            state.opening = None;
            match result {
                Ok(snapshot) if snapshot.workspace == workspace => {
                    state.route = EntryRoute::Home(Box::new(AppState::home(
                        snapshot.workspace,
                        snapshot.sessions,
                    )));
                    state.failed = None;
                    state.error = None;
                }
                Ok(_) => {
                    state.failed = Some(workspace);
                    state.error = Some(Notice::new("workspace changed while opening; retry"));
                }
                Err(error) => {
                    state.failed = Some(workspace);
                    state.error = Some(error);
                }
            }
            Vec::new()
        }
        // A late completion, an invalid selection, an empty list, and keys that
        // do not apply to this screen have no observable state transition.
        _ => Vec::new(),
    }
}

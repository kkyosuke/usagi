//! Director drawer / Director tab の選択と projection。

use super::{
    AgentRuntimeId, AgentTabIntent, AgentTabIntentMutation, AppEvent, AppKey, DirectorConversation,
    DirectorDrawerProjection, DirectorNew, DirectorNewProjection, DirectorOrganizationRow,
    DirectorRoute, Effect, HomeProjection, Key, LiveTerminalAction, OperationId, PaneKind, PaneTab,
    PointerEvent, PointerKind, SessionId, SupervisorRunId, TabSelection, Target,
    TerminalViewProjection, WorkRunControlMode, WorkRunControlProjection, WorkRunProjection,
    WorkspaceDrawerFocus, WorkspaceForegroundInputOwner, WorkspaceIoRuntime, WorkspaceRuntime,
    activate_focused_interrupted_tab, drawer_agent_owns_escape, surface_agent_tab_intent_error,
    workspace_drawer_header_key, workspace_foreground_input_owner,
};

/// Route an input owned by the Director picker. Resize and runtime wake events
/// are not user input and keep flowing so geometry and backend progress cannot
/// stall behind the foreground owner.
pub(super) fn handle_director_picker_input(
    runtime: &mut WorkspaceRuntime,
    key: &Key,
) -> Option<Vec<Effect>> {
    if workspace_foreground_input_owner(runtime) == WorkspaceForegroundInputOwner::DirectorPicker {
        // Organization is management-owned, but its Director selector lives at
        // the pane seam because changing a row also persists exact Agent intent.
        // Let only those navigation keys reach `select_director_tab`; every
        // other non-Console input remains exclusive to the drawer.
        if matches!(runtime.state().director_new(), DirectorNew::Idle)
            && runtime.state().director_route() == DirectorRoute::Organization
            && matches!(
                key,
                Key::Up
                    | Key::Down
                    | Key::Live(LiveTerminalAction::PreviousTab | LiveTerminalAction::NextTab)
            )
        {
            return None;
        }
        return match key {
            Key::Resize | Key::Other => None,
            _ => Some(runtime.handle_key(key.clone())),
        };
    }
    // Console is not exclusive because its selected root Agent owns ordinary
    // terminal input. Its drawer-local open/close operations still precede PTY
    // forwarding and the Home reducer.
    if runtime.state().overlay().is_none()
        && runtime.state().director_drawer_open()
        && (matches!(
            key,
            Key::Live(LiveTerminalAction::Director | LiveTerminalAction::DirectorNew)
        ) || (matches!(key, Key::Escape) && !drawer_agent_owns_escape(runtime)))
    {
        Some(runtime.handle_key(key.clone()))
    } else {
        None
    }
}

pub(super) fn select_director_agent(
    runtime_id: AgentRuntimeId,
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
) -> bool {
    let Some(index) = runtime.agent_tab_index(runtime_id, ui.agent_inventory()) else {
        return false;
    };
    let selection = runtime
        .tab_selection_at(index)
        .expect("an Agent index is returned only for a tab in the same pane");
    select_director_selection(selection, ui, runtime)
}

pub(super) fn select_director_selection(
    selection: TabSelection,
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
) -> bool {
    let continuation = match &selection {
        TabSelection::Live(terminal) => ui.agent_continuation_for(terminal),
        TabSelection::Interrupted(continuation) => Some(*continuation),
        TabSelection::Pending(_) | TabSelection::Ready(_) => return false,
    };
    if ui
        .mutate_agent_intent(AgentTabIntentMutation::Select {
            session_id: None,
            continuation,
        })
        .is_err()
    {
        return false;
    }
    let _ = runtime.select_tab_selection(selection);
    true
}

/// Project the root pane entry into the frontmost Agent-only drawer.
///
/// Stable identity and selection remain in the pane/intent reducers. This
/// adapter exposes only safe labels and the already-rendered VT rows.
pub(super) fn director_drawer_projection(
    ui: &WorkspaceIoRuntime,
    runtime: &WorkspaceRuntime,
    terminal_view: Option<&TerminalViewProjection>,
) -> DirectorDrawerProjection {
    if !runtime.state().director_drawer_open() {
        return DirectorDrawerProjection::default();
    }
    let pane = runtime.active_pane();
    let selected = runtime.director_selection();
    let mut conversations = Vec::new();
    for tab in pane.tabs() {
        let conversation = match tab {
            PaneTab::Live(live) if live.kind == PaneKind::Agent => Some(DirectorConversation {
                label: AgentTabIntent::safe_label_or_fallback(
                    ui.agent_continuation_for(&live.terminal),
                ),
                selected: matches!(
                    selected.as_ref(),
                    Some(TabSelection::Live(terminal)) if terminal.fences(&live.terminal)
                ),
            }),
            PaneTab::Interrupted(interrupted) => Some(DirectorConversation {
                label: interrupted.tab.safe_label(),
                selected: matches!(
                    selected.as_ref(),
                    Some(TabSelection::Interrupted(continuation))
                        if *continuation == interrupted.tab.continuation
                ),
            }),
            PaneTab::Pending(pending) if pending.kind == PaneKind::Agent => {
                Some(DirectorConversation {
                    label: "Agent (starting)".to_owned(),
                    selected: matches!(
                        selected.as_ref(),
                        Some(TabSelection::Pending(operation)) if *operation == pending.operation
                    ),
                })
            }
            PaneTab::Live(_) | PaneTab::Pending(_) | PaneTab::Ready(_) => None,
        };
        if let Some(conversation) = conversation {
            conversations.push(conversation);
        }
    }
    let mut terminal_view = terminal_view.cloned();
    if let Some(view) = &mut terminal_view
        && view.feedback.is_none()
    {
        view.feedback = pane.error().map(str::to_owned);
    }
    let interrupted_detail = if terminal_view.is_none() {
        runtime
            .focused_interrupted()
            .map(|interrupted| interrupted.safe_detail().to_owned())
    } else {
        None
    };
    // Management routes do not draw `terminal_view`, but a root launch can
    // fail while an older live Director remains selected. Keep the pane-safe
    // reason at the route level as well as in the Console terminal projection.
    let feedback = pane.error().map(str::to_owned);
    let goal_driven =
        runtime.state().work_mode() == usagi_core::domain::settings::WorkMode::GoalDriven;
    let organization = if !goal_driven
        && conversations
            .iter()
            .any(|conversation| conversation.selected)
    {
        director_organization(ui)
    } else {
        Vec::new()
    };
    if goal_driven {
        conversations.clear();
    }
    DirectorDrawerProjection {
        focused: runtime.state().workspace_drawer_focus() == Some(WorkspaceDrawerFocus::Director),
        route: runtime.state().director_route(),
        goal_driven,
        conversations,
        organization,
        terminal_view,
        interrupted_detail,
        feedback,
        new: director_new_projection(runtime),
        work_runs: WorkRunProjection::default(),
        work_run_control: WorkRunControlProjection::default(),
    }
}

pub(super) fn director_new_projection(runtime: &WorkspaceRuntime) -> DirectorNewProjection {
    if runtime.state().director_launching().is_some() {
        return DirectorNewProjection::Launching;
    }
    match runtime.state().director_new() {
        DirectorNew::Idle => DirectorNewProjection::Ready,
        DirectorNew::Empty => DirectorNewProjection::Empty,
        DirectorNew::Choosing(selected) => {
            let candidates = runtime
                .state()
                .available_models()
                .iter()
                .map(|model| model.selector().to_owned())
                .collect::<Vec<_>>();
            let selected = runtime
                .state()
                .available_models()
                .iter()
                .position(|model| model == selected)
                .unwrap_or(0);
            if runtime.state().work_mode() == usagi_core::domain::settings::WorkMode::GoalDriven {
                DirectorNewProjection::GoalComposer {
                    candidates,
                    selected,
                    goal: runtime.state().director_goal().to_owned(),
                }
            } else {
                DirectorNewProjection::Choosing {
                    candidates,
                    selected,
                }
            }
        }
    }
}

pub(super) fn director_organization(ui: &WorkspaceIoRuntime) -> Vec<DirectorOrganizationRow> {
    fn append_children(
        parent: Option<SessionId>,
        depth: usize,
        members: &[(SessionId, Option<SessionId>, DirectorOrganizationRow)],
        emitted: &mut std::collections::BTreeSet<SessionId>,
        rows: &mut Vec<DirectorOrganizationRow>,
    ) {
        for (id, member_parent, row) in members {
            if *member_parent == parent && emitted.insert(*id) {
                let mut row = row.clone();
                row.depth = depth;
                rows.push(row);
                append_children(Some(*id), depth.saturating_add(1), members, emitted, rows);
            }
        }
    }

    let roles = ui.workspace.session_roles();
    let mut members = Vec::new();
    for (session_id, session) in ui
        .workspace
        .session_ids()
        .iter()
        .zip(ui.workspace.sessions())
    {
        let role_identity = roles
            .get(session_id)
            .and_then(|role| role.role_id.as_ref())
            .map_or_else(
                || "• Executor".to_owned(),
                |role| super::views::workspace::role_identity(role.as_str()),
            );
        let status = match roles.get(session_id).and_then(|role| role.agent_status) {
            Some(usagi_core::domain::agent::AgentStatus::Starting) => "starting",
            Some(usagi_core::domain::agent::AgentStatus::Running) => "running",
            Some(usagi_core::domain::agent::AgentStatus::Idle) => "waiting",
            Some(usagi_core::domain::agent::AgentStatus::Exited) => "stopped",
            Some(usagi_core::domain::agent::AgentStatus::Failed) => "failed",
            None => "ready",
        };
        let row = DirectorOrganizationRow {
            depth: 0,
            label: format!("{role_identity} · {}", session.name),
            status: status.to_owned(),
        };
        members.push((
            *session_id,
            roles
                .get(session_id)
                .and_then(|role| role.parent_session_id),
            row,
        ));
    }
    let mut rows = vec![DirectorOrganizationRow {
        depth: 0,
        label: format!("{} Director", super::director_drawer::DIRECTOR_ICON),
        status: "active".into(),
    }];
    let mut emitted = std::collections::BTreeSet::new();
    append_children(None, 1, &members, &mut emitted, &mut rows);
    // Corrupt or retention-truncated parentage is still visible, but never
    // allowed to form an unbounded/cyclic presentation walk.
    for (id, _, row) in members {
        if emitted.insert(id) {
            let mut row = row;
            row.depth = 1;
            rows.push(row);
        }
    }
    rows
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DirectorTabSelection {
    Unhandled,
    Handled,
    Selected,
}

pub(super) fn select_director_tab_outcome(
    key: &Key,
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
) -> DirectorTabSelection {
    if !runtime.state().director_drawer_open() {
        return DirectorTabSelection::Unhandled;
    }
    let direction = match key {
        Key::Live(LiveTerminalAction::NextTab) => {
            crate::usecase::application::controller::TabDirection::Next
        }
        Key::Live(LiveTerminalAction::PreviousTab) => {
            crate::usecase::application::controller::TabDirection::Previous
        }
        Key::Down if runtime.state().director_route() == DirectorRoute::Organization => {
            crate::usecase::application::controller::TabDirection::Next
        }
        Key::Up if runtime.state().director_route() == DirectorRoute::Organization => {
            crate::usecase::application::controller::TabDirection::Previous
        }
        _ => return DirectorTabSelection::Unhandled,
    };
    let Some(selection) = runtime.agent_selection_after_select(direction) else {
        return DirectorTabSelection::Handled;
    };
    let continuation = match &selection {
        TabSelection::Live(terminal) => ui.agent_continuation_for(terminal),
        TabSelection::Interrupted(continuation) => Some(*continuation),
        TabSelection::Pending(_) | TabSelection::Ready(_) => None,
    };
    match ui.mutate_agent_intent(AgentTabIntentMutation::Select {
        session_id: None,
        continuation,
    }) {
        Ok(()) => {
            let _ = runtime.select_tab_selection(selection);
            DirectorTabSelection::Selected
        }
        Err(error) => {
            surface_agent_tab_intent_error(runtime, error);
            DirectorTabSelection::Handled
        }
    }
}

#[cfg(test)]
pub(super) fn select_director_tab(
    key: &Key,
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
) -> bool {
    select_director_tab_outcome(key, ui, runtime) != DirectorTabSelection::Unhandled
}

pub(super) fn select_director_tab_and_activate(
    key: &Key,
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    pending_targets: &mut std::collections::HashMap<OperationId, Target>,
) -> bool {
    let outcome = select_director_tab_outcome(key, ui, runtime);
    if outcome == DirectorTabSelection::Selected
        && matches!(runtime.state().director_route(), DirectorRoute::Console(_))
    {
        activate_focused_interrupted_tab(ui, runtime, pending_targets);
    }
    outcome != DirectorTabSelection::Unhandled
}

pub(super) fn is_director_new_click(
    key: &Key,
    runtime: &WorkspaceRuntime,
    height: usize,
    width: usize,
) -> bool {
    let (column, row) = match key {
        Key::Click { column, row }
        | Key::Pointer(PointerEvent {
            kind: PointerKind::Down,
            column,
            row,
        }) => (*column, *row),
        _ => return false,
    };
    runtime.state().overlay().is_none()
        && runtime.state().director_drawer_open()
        && runtime.state().workspace_drawer_focus() == Some(WorkspaceDrawerFocus::Director)
        && matches!(runtime.state().director_new(), DirectorNew::Idle)
        && runtime.state().director_launching().is_none()
        && super::director_drawer::new_button_at(
            height,
            width,
            column,
            row,
            runtime.state().work_mode() == usagi_core::domain::settings::WorkMode::GoalDriven,
            false,
        )
}

/// Resolve the Director's visible New / Start button before route-local input.
///
/// Work Runs deliberately owns otherwise-unrecognized clicks, so deferring
/// this chrome action would make the primary Goal-driven CTA inert.
pub(super) fn open_director_from_new_button(
    runtime: &mut WorkspaceRuntime,
    key: &Key,
    height: usize,
    width: usize,
    work_run_mode: WorkRunControlMode,
) -> Option<Vec<Effect>> {
    (work_run_mode != WorkRunControlMode::Submitting
        && is_director_new_click(key, runtime, height, width))
    .then(|| runtime.apply_event(AppEvent::Key(AppKey::OpenDirectorNew)))
}

/// Apply a workspace-drawer header button while Director is open, before its
/// exclusive picker consumes the press. Existing modals keep precedence, and a
/// closed Director continues through the ordinary Home header route.
pub(super) fn apply_drawer_header_while_director_open(
    runtime: &mut WorkspaceRuntime,
    key: &Key,
    width: usize,
    home: &HomeProjection,
) -> Option<Vec<Effect>> {
    if runtime.state().overlay().is_some() || !runtime.state().director_drawer_open() {
        return None;
    }
    let key = workspace_drawer_header_key(key, width, home)?;
    Some(runtime.apply_event(AppEvent::Key(key)))
}

pub(super) fn is_director_new_pointer(
    key: &Key,
    runtime: &WorkspaceRuntime,
    height: usize,
    width: usize,
) -> bool {
    let (column, row) = match key {
        Key::Click { column, row } | Key::Pointer(PointerEvent { column, row, .. }) => {
            (*column, *row)
        }
        _ => return false,
    };
    runtime.state().director_drawer_open()
        && super::director_drawer::new_button_at(
            height,
            width,
            column,
            row,
            runtime.state().work_mode() == usagi_core::domain::settings::WorkMode::GoalDriven,
            false,
        )
}

pub(super) fn complete_director_launch(
    runtime: &mut WorkspaceRuntime,
    target: Target,
    operation: OperationId,
    supervisor_run_id: Option<SupervisorRunId>,
    succeeded: bool,
) {
    if matches!(target, Target::Root(_)) {
        let _ = runtime.apply_event(AppEvent::DirectorLaunchFinished {
            operation,
            supervisor_run_id,
            succeeded,
        });
    }
}

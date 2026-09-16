//! Garden 画面の入力 routing と observation job。
//!
//! Home frame loop は「どこへ入力を渡すか」だけを見て、Garden 固有の判断は
//! ここが持つ。

use super::{
    AgentRuntimeId, AgentWorkspaceObservation, AppEvent, Effect, GardenClick, GardenInventoryPort,
    HomeFrameMaterial, Key, LiveTerminalAction, MAX_OBSERVED_PROJECTS, Overlay, PointerEvent,
    PointerKind, Sender, SessionId, WorkspaceId, WorkspaceIoRuntime, WorkspaceRuntime,
    garden_click_at, is_user_activity, select_right_pane_tab,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct GardenProjectVisit {
    pub(super) workspace: WorkspaceId,
    pub(super) session: SessionId,
    pub(super) agent: Option<AgentRuntimeId>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum GardenInputRoute {
    Local(Vec<Effect>),
    Agent(Vec<Effect>),
    Project(GardenProjectVisit),
}

pub(super) fn garden_scroll_input(
    material: Option<&HomeFrameMaterial>,
    key: &Key,
) -> Option<GardenClick> {
    let scroll = |lines, position| {
        material
            .and_then(|material| {
                super::views::workspace::garden_scroll_at(
                    material.height,
                    material.width,
                    &material.projection,
                    material.now,
                    lines,
                    position,
                )
            })
            .unwrap_or(GardenClick::Dismiss)
    };
    let page = material.map_or(1, |material| {
        isize::try_from(material.height.saturating_sub(3)).unwrap_or(isize::MAX)
    });
    Some(match key {
        Key::Up => scroll(-1, None),
        Key::Down => scroll(1, None),
        Key::PageUp => scroll(-page, None),
        Key::PageDown => scroll(page, None),
        Key::Live(LiveTerminalAction::Wheel {
            up,
            column,
            row,
            notches,
        }) => {
            let lines = isize::try_from(*notches).unwrap_or(isize::MAX);
            scroll(if *up { -lines } else { lines }, Some((*column, *row)))
        }

        _ => return None,
    })
}

/// Give an open Garden exclusive ownership of the next user interaction.
/// Backend/frame ticks are not user activity and keep the screen saver open.
/// A pointer press also fences the rest of its drag/release gesture so closing
/// the Garden cannot leak that tail into the terminal selection underneath.
pub(super) fn route_garden_input(
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    material: Option<&HomeFrameMaterial>,
    key: &Key,
    pointer_gesture: &mut bool,
) -> Option<GardenInputRoute> {
    if *pointer_gesture && matches!(key, Key::Pointer(_)) {
        if matches!(
            key,
            Key::Pointer(PointerEvent {
                kind: PointerKind::Up,
                ..
            })
        ) {
            *pointer_gesture = false;
        }
        return Some(GardenInputRoute::Local(Vec::new()));
    }
    if let Key::Pointer(PointerEvent {
        kind: PointerKind::Move,
        column,
        row,
    }) = key
    {
        return Some(GardenInputRoute::Local(runtime.apply_event(
            AppEvent::GardenHover {
                column: *column,
                row: *row,
            },
        )));
    }
    if runtime.state().overlay() != Some(Overlay::Garden) || !is_user_activity(key) {
        return None;
    }

    if let Some(click) = garden_scroll_input(material, key) {
        return Some(GardenInputRoute::Local(
            runtime.apply_event(AppEvent::GardenClick(click)),
        ));
    }
    let pointer = match key {
        Key::Click { column, row } => material
            .and_then(|material| {
                garden_click_at(
                    material.height,
                    material.width,
                    &material.projection,
                    material.now,
                    *column,
                    *row,
                )
            })
            .unwrap_or(GardenClick::Dismiss),
        Key::Pointer(PointerEvent {
            kind: PointerKind::Down,
            column,
            row,
        }) => {
            *pointer_gesture = true;
            material
                .and_then(|material| {
                    garden_click_at(
                        material.height,
                        material.width,
                        &material.projection,
                        material.now,
                        *column,
                        *row,
                    )
                })
                .unwrap_or(GardenClick::Dismiss)
        }
        Key::Pointer(PointerEvent {
            kind: PointerKind::Drag,
            ..
        }) => {
            *pointer_gesture = true;
            GardenClick::Dismiss
        }
        _ => GardenClick::Dismiss,
    };
    let effects = runtime.apply_event(AppEvent::GardenClick(pointer));
    if let GardenClick::Visit {
        workspace,
        session,
        agent,
    } = pointer
        && workspace != runtime.state().workspace()
    {
        return Some(GardenInputRoute::Project(GardenProjectVisit {
            workspace,
            session,
            agent,
        }));
    }
    Some(if visit_garden_agent(ui, runtime, pointer) {
        GardenInputRoute::Agent(effects)
    } else {
        GardenInputRoute::Local(effects)
    })
}

pub(super) fn garden_shell_owned_wake(key: &Key) -> bool {
    // List input needs the drawn viewport before it can either scroll or wake
    // a narrow Garden. Leave it for route_garden_input, just like hit-tested clicks.
    !matches!(
        key,
        Key::Pointer(PointerEvent {
            kind: PointerKind::Move,
            ..
        }) | Key::Click { .. }
            | Key::Up
            | Key::Down
            | Key::PageUp
            | Key::PageDown
            | Key::Live(LiveTerminalAction::Wheel { .. })
            | Key::Other
            | Key::Resize
    )
}

pub(super) struct UnavailableGardenInventoryPort;

impl GardenInventoryPort for UnavailableGardenInventoryPort {
    fn inventory(&mut self, _: WorkspaceId) -> Result<AgentWorkspaceObservation, String> {
        Err("Agent inventory is unavailable".to_owned())
    }
}

/// One completed round of the Garden's cross-project observation.
pub(super) struct GardenObservationCompletion {
    pub(super) port: Box<dyn GardenInventoryPort>,
    /// Inventories the daemon answered, each already checked to be the
    /// workspace it was asked for.
    pub(super) inventories: Vec<AgentWorkspaceObservation>,
}

/// Observe the other open projects' Agent inventory off the frame thread.
///
/// A workspace whose daemon does not answer, or answers with another
/// workspace's inventory, is skipped: the Garden keeps that project's read-only
/// plot rather than drawing a foreign project's rabbits in it.
pub(super) fn spawn_garden_observation_job(
    mut port: Box<dyn GardenInventoryPort>,
    workspaces: Vec<WorkspaceId>,
    sender: Sender<GardenObservationCompletion>,
) {
    std::thread::spawn(move || {
        let mut inventories = Vec::new();
        for workspace in workspaces.into_iter().take(MAX_OBSERVED_PROJECTS) {
            if let Ok(inventory) = port.inventory(workspace)
                && inventory.inventory.workspace_id == workspace
            {
                inventories.push(inventory);
            }
        }
        let _ = sender.send(GardenObservationCompletion { port, inventories });
    });
}

/// Select one visible managed-session tab after the frame's hit test resolved
/// its display index. Agent selection is committed before registry mutation,
/// matching keyboard tab cycling's durability fence.
/// Focus the Agent tab of the rabbit a Garden click landed on.
///
/// The Garden itself owns no target semantics beyond the session activation the
/// reducer already performed ([`GardenClick`]); this only moves the selection
/// inside the Closeup that activation opened, through the same stable-identity
/// path a click on the tab strip uses.
pub(super) fn visit_garden_agent(
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    click: GardenClick,
) -> bool {
    let GardenClick::Visit {
        agent: Some(runtime_id),
        ..
    } = click
    else {
        return false;
    };
    if !runtime.wants_right_pane_tab_click() {
        return false;
    }
    let Some(index) = runtime.agent_tab_index(runtime_id, ui.agent_inventory()) else {
        return false;
    };
    select_right_pane_tab(ui, runtime, index)
}

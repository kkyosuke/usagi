//! 実端末の Home frame loop（drain → poll → render → input → dispatch）と screen graph の起動。

use std::io;
use std::sync::mpsc;

use super::{
    AgentTabIntentMutation, AppEvent, AppKey, AppState, Arc, AvailableAgentModels, BTreeMap,
    BTreeSet, BackendEvent, BackendFlow, Color, Completions, Config, ConfigStep, ConfirmationView,
    ControllerBackendFactory, ControllerHost, ControllerHostAction, DateTime, Effect, EntryForm,
    Exit, FRAME_EVENT_BUDGET, Feedback, GARDEN_OBSERVATION_BACKOFF, GARDEN_OBSERVATION_INTERVAL,
    GardenClick, GardenInputRoute, GardenObservationCompletion, Geometry, GitDiff,
    HomeFrameMaterial, HomeHeaderAction, HomeMode, HomeProjection, IconMode, IdleWatch, Key,
    LiveTerminalAction, LiveTerminalControls, MetricsBackend, MetricsProjection,
    MissingWorkspacePrompt, New, NewStep, Notice, ObservationLane, Open, OpenStep, OperationId,
    Overlay, OverlayIntent, PROJECT_BAR_ROWS, PaneLaunch, PaneTab, Path, PendingCreate,
    PendingWorkspaceCreate, PointerEvent, PointerKind, PrModalClickRoute, ProjectBarTarget,
    ProjectedSession, REGISTRY_REFRESH_INTERVAL, Receiver, Recent, RestoreJobOutcome,
    RestoreRetryState, Route, Screen, SessionBackendCompletion, SessionCommand, SessionId,
    SessionRefreshPort, SessionWorktreeHint, SettingsPort, Start, Style, TabSelection, Target,
    Terminal, TerminalRef, TerminalViewProjection, Utc, WORK_RUN_OBSERVATION_BACKOFF,
    WORK_RUN_OBSERVATION_INTERVAL, Welcome, WelcomeStep, WorkMode, WorkRunControl,
    WorkRunControlInput, WorkRunControlOutcome, WorkRunControlResult, WorkRunLaneCompletion,
    WorkRunProjection, Workspace, WorkspaceConfigContext, WorkspaceCreateEffect,
    WorkspaceCreateToken, WorkspaceDeck, WorkspaceDrawerFocus, WorkspaceEntryPolicy,
    WorkspaceInputRoute, WorkspaceIoRuntime, WorkspaceLoader, WorkspaceRuntime, WorkspaceSnapshot,
    WorkspaceStep, WorkspaceView, activate_focused_interrupted_tab, activate_workspace_responsive,
    adjust_project_bar_pointer, apply_drawer_header_while_director_open, apply_restore_completion,
    begin_session_command, close_exited_panes, closes_workspace_help,
    compose_workspace_shell_frame, controller_terminal_view, director_drawer_projection,
    dismiss_pr_modal_on_project_bar_click, drain_pane_completions_into_runtime,
    drain_pane_launches, drain_session_completions, drain_session_refresh, enqueue_pane_launch,
    enter_workspace, enter_workspace_deck, entry_help_context, fail_terminal_launch,
    focus_workspace_drawer_from_pointer, foreground_terminal_geometry, garden_fits,
    garden_shell_owned_wake, handle_interrupted_removal_confirmation,
    handle_work_run_control_input_with_ui, home_header_action_at, intercept_live_terminal_control,
    managed_background_terminal, managed_background_terminal_geometry, new_project_notice,
    open_director_from_new_button, open_failure_notice, open_from_registry, opens_workspace_help,
    prepare_activation_settings, prepare_batch_settings, prepare_deck_workspace,
    prepare_workspace_deck, project_bar, project_controller_sessions, recent_paths,
    registry_contains_path, relative_time_clock, remember_workspace_session_focus,
    remove_registry_paths, render_home, render_home_at, render_missing_workspace_prompt,
    render_open, restore_workspace_closeup, restore_workspace_session_focus,
    retarget_drawer_chords, right_pane_tab_at, route_garden_input, route_pr_modal_click,
    route_workspace_input_before_reducer, run_workspace_config, save_config_responsive,
    save_config_source_responsive, scroll_key_help, select_right_pane_tab, session_name_for,
    sidebar_pointer_event, spawn_garden_observation_job, spawn_restore_job,
    spawn_work_run_control_job, spawn_work_run_observation_job, step_config, step_new, step_open,
    step_welcome, surface_agent_tab_intent_error, sync_runtime_sessions,
    sync_terminal_selection_motions, visit_garden_agent, work_run_control_projection,
    workspace_has_unsaved_surface, workspace_help_context, workspace_navigation_target,
    workspace_terminal_attachments,
};

/// Render a single static Home frame from a workspace snapshot, using the same
/// controller projection as the interactive loop.
///
/// This is the non-interactive `usagi open <path>` fallback (no terminal), so
/// it shows the initial project bar and Home surface: root selected/active, the
/// snapshot's sessions, and the `+ new session` row. The composition root passes
/// the resolved global icon preference just as it does for the interactive loop.
#[must_use]
pub fn render_home_snapshot(
    height: usize,
    width: usize,
    snapshot: &WorkspaceSnapshot,
    icon_mode: IconMode,
) -> Vec<String> {
    let (height, width) = super::widgets::normalize_size(height, width);
    let workspace = WorkspaceView::with_runtime_ids(
        snapshot.workspace.clone(),
        snapshot.state.clone(),
        snapshot.session_ids.clone(),
    );
    let sessions: Vec<ProjectedSession> = workspace
        .sessions()
        .iter()
        .zip(workspace.session_ids())
        .map(|(record, id)| {
            let mut projected = ProjectedSession::from_record(*id, record);
            if let Some(projection) = snapshot.session_lifecycles.get(id) {
                projected.lifecycle = projection.lifecycle;
                projected
                    .failure_stage
                    .clone_from(&projection.failure_stage);
                projected
                    .failure_summary
                    .clone_from(&projection.failure_summary);
            }
            projected
        })
        .collect();
    let state = AppState::home(snapshot.workspace_id, snapshot.session_ids.clone());
    let projection = HomeProjection::from_state(&state, &snapshot.workspace.name, &sessions)
        .with_icon_mode(icon_mode);
    let mut frame = Vec::with_capacity(height);
    frame.push(project_bar(&WorkspaceDeck::new(snapshot), width).line);
    frame.extend(render_home(
        height.saturating_sub(PROJECT_BAR_ROWS),
        width,
        &projection,
    ));
    frame
}

/// Cheap dependency vector for the owned Home projection. Equality is the
/// admission gate to projection construction; each revision is advanced by its
/// authoritative controller/daemon source, never by this cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FrameMaterialKey {
    pub(super) height: usize,
    pub(super) width: usize,
    pub(super) controller: (u64, u64),
    pub(super) sessions: (u64, Option<SessionId>, u64),
    pub(super) shell: u64,
    pub(super) metrics: u64,
    pub(super) terminal: (u64, u64, u64, u64),
    pub(super) animation: u64,
    pub(super) create_pending: Option<String>,
    /// Rounds of the Garden's cross-project observation that changed a plot.
    /// The other projects' rabbits are draw material this loop owns, so their
    /// change has to reach the key that admits a rebuild.
    pub(super) garden_observations: u64,
    pub(super) work_run_revision: u64,
    pub(super) now: DateTime<Utc>,
}

impl FrameMaterialKey {
    /// Only controller admission can conservatively advance for a reducer no-op
    /// (for example Escape on the base route). Every other generation denotes
    /// changed draw material and can bypass an owned-projection equality scan.
    fn differs_only_by_controller(&self, other: &Self) -> bool {
        self.controller != other.controller
            && self.height == other.height
            && self.width == other.width
            && self.sessions == other.sessions
            && self.shell == other.shell
            && self.metrics == other.metrics
            && self.terminal == other.terminal
            && self.animation == other.animation
            && self.create_pending == other.create_pending
            && self.garden_observations == other.garden_observations
            && self.work_run_revision == other.work_run_revision
            && self.now == other.now
    }
}

#[allow(clippy::too_many_arguments)] // 注入された port をそのまま受け取る composition 境界で、束ねると呼び手が構造体を組むだけになる。
pub(super) fn home_frame_material(
    height: usize,
    width: usize,
    runtime: &WorkspaceRuntime,
    workspace_name: &str,
    sessions: &[ProjectedSession],
    metrics: Option<usagi_core::infrastructure::ipc::DaemonMetrics>,
    health: crate::usecase::application::daemon_health::DaemonHealthTracker,
    git_diffs: &BTreeMap<SessionId, GitDiff>,
    terminal_view: Option<TerminalViewProjection>,
    create_pending: Option<&str>,
    now: DateTime<Utc>,
) -> HomeFrameMaterial {
    let (managed_terminal_view, root_terminal_view) =
        if runtime.state().workspace_drawer_focus() == Some(WorkspaceDrawerFocus::Terminal) {
            (None, terminal_view.map(Arc::new))
        } else {
            (terminal_view.map(Arc::new), None)
        };
    home_frame_material_shared(
        height,
        width,
        runtime,
        workspace_name,
        Arc::from(sessions.to_vec()),
        metrics,
        health,
        Arc::new(git_diffs.clone()),
        managed_terminal_view,
        root_terminal_view.as_deref(),
        create_pending,
        now,
        usagi_core::domain::settings::IconMode::default(),
    )
    .with_garden_animation(
        super::widgets::garden::runtime_tick(runtime.state().mascot_tick()),
        false,
    )
}

#[allow(clippy::too_many_arguments)] // 注入された port をそのまま受け取る composition 境界で、束ねると呼び手が構造体を組むだけになる。
pub(super) fn home_frame_material_shared(
    height: usize,
    width: usize,
    runtime: &WorkspaceRuntime,
    workspace_name: &str,
    sessions: Arc<[ProjectedSession]>,
    metrics: Option<usagi_core::infrastructure::ipc::DaemonMetrics>,
    health: crate::usecase::application::daemon_health::DaemonHealthTracker,
    git_diffs: Arc<BTreeMap<SessionId, GitDiff>>,
    managed_terminal_view: Option<Arc<TerminalViewProjection>>,
    root_terminal_view: Option<&TerminalViewProjection>,
    create_pending: Option<&str>,
    now: DateTime<Utc>,
    icon_mode: usagi_core::domain::settings::IconMode,
) -> HomeFrameMaterial {
    let force_remove_confirmation =
        runtime
            .state()
            .force_remove_confirmation()
            .and_then(|(target, confirm)| {
                sessions
                    .iter()
                    .find(|session| session.id == target)
                    .map(|session| (session.label.clone(), confirm))
            });
    let root_terminal_projection = runtime.root_terminal_projection(root_terminal_view);
    let projection = HomeProjection::from_ordered_state(runtime.state(), workspace_name, sessions)
        .with_icon_mode(icon_mode)
        .with_pane(runtime.preview_pane())
        .with_metrics(metrics)
        // Diagnostic-only material. It rides the frame material like every
        // other renderer input, so an idle Home still skips redraws.
        .with_health(health)
        .with_shared_git_diffs(git_diffs)
        .with_shared_terminal_view(managed_terminal_view)
        .with_director_drawer(runtime.director_projection().clone())
        .with_root_terminal_drawer(root_terminal_projection)
        .with_create_pending(create_pending.map(str::to_owned))
        .with_overlay_modals(
            runtime.overview_modal().cloned(),
            runtime.closeup_modal().cloned(),
        )
        // Last, once every surface that reads the animation clock is known.
        .collapse_animation_clock();
    HomeFrameMaterial {
        height,
        width,
        projection,
        interrupted_removal_confirmation: runtime.interrupted_removal_confirmation().map(
            |confirmation| {
                (
                    confirmation.tab().safe_label(),
                    confirmation.tab().safe_detail().to_owned(),
                    confirmation.is_confirm_selected(),
                )
            },
        ),
        quit_confirmation: (runtime.state().overlay() == Some(Overlay::QuitConfirmation))
            .then(|| runtime.state().exit_choice()),
        create_error: runtime
            .state()
            .create_session_error()
            .map(|error| error.message.clone()),
        terminal_launch_error: runtime
            .state()
            .terminal_launch_error()
            .map(|error| error.message.clone()),
        agent_launch_error: runtime
            .state()
            .agent_launch_error()
            .map(|error| error.message.clone()),
        force_remove_confirmation,
        environment_editor: runtime.state().environment_editor().cloned(),
        role_editor: runtime.state().role_editor().cloned(),
        // Garden canonicalization happens only after every composition-owned
        // source (notably Agent inventory and reduced motion) is attached.
        now: relative_time_clock(now),
    }
}

/// Compose the controller Home frame: [`render_home_at`] plus the shell
/// overlays it does not own (quit confirmation, create-failure dialog).
pub(super) fn render_home_material(material: &HomeFrameMaterial) -> Vec<String> {
    let frame = render_home_at(
        material.height,
        material.width,
        &material.projection,
        material.now,
    );
    // The create form renders inline in the `+ new session` sidebar row (see
    // `render_home`), so no overlay composite is needed here.
    if let Some((label, message, confirm)) = &material.interrupted_removal_confirmation {
        let title = Style::new()
            .fg(Color::White)
            .bold()
            .paint("Remove interrupted Agent");
        let heading = Style::new()
            .fg(Color::White)
            .bold()
            .paint(&format!("Remove {label}?"));
        let mut view = ConfirmationView::confirmation(&title, 60, heading, message);
        view.confirm_label = "remove";
        view.cancel_label = "keep";
        view.hints = "Enter: select   y: remove   Esc/n: keep   ←→/Tab: move";
        return super::widgets::modal::render_confirmation_over(
            material.height,
            material.width,
            &frame,
            super::widgets::modal::ConfirmationModal::from_confirm_selected(*confirm),
            view,
        );
    }
    if let Some(choice) = material.quit_confirmation {
        return super::views::quit_modal::render_over(
            material.height,
            material.width,
            &frame,
            choice,
        );
    }
    if let Some(message) = &material.create_error {
        return super::views::create_session_error_modal::render_over(
            material.height,
            material.width,
            &frame,
            message,
        );
    }
    if let Some(message) = &material.terminal_launch_error {
        return super::views::create_session_error_modal::render_titled_over(
            material.height,
            material.width,
            &frame,
            "Terminal failed to open",
            message,
        );
    }
    if let Some(message) = &material.agent_launch_error {
        return super::views::create_session_error_modal::render_titled_over(
            material.height,
            material.width,
            &frame,
            "Agent failed to start",
            message,
        );
    }
    if let Some((label, confirm)) = &material.force_remove_confirmation {
        let title = Style::new().fg(Color::White).bold().paint("Force remove");
        let heading = Style::new()
            .fg(Color::White)
            .bold()
            .paint(&format!("Force remove {label}?"));
        return super::widgets::modal::render_confirmation_over(
            material.height,
            material.width,
            &frame,
            super::widgets::modal::ConfirmationModal::from_confirm_selected(*confirm),
            ConfirmationView::confirmation(
                &title,
                52,
                heading,
                "Previous removal failed. Changes may be discarded.",
            ),
        );
    }
    if let Some(editor) = &material.environment_editor {
        return super::views::scratchpad_modal::render_environment_over(
            material.height,
            material.width,
            &frame,
            editor,
        );
    }
    if let Some(editor) = &material.role_editor {
        let height = material.height;
        let width = material.width;
        return super::views::scratchpad_modal::render_roles_over(height, width, &frame, editor);
    }
    frame
}

#[allow(clippy::too_many_arguments)] // 注入された port をそのまま受け取る composition 境界で、束ねると呼び手が構造体を組むだけになる。
pub(super) fn render_controller_frame(
    height: usize,
    width: usize,
    runtime: &WorkspaceRuntime,
    workspace_name: &str,
    sessions: &[ProjectedSession],
    metrics: Option<usagi_core::infrastructure::ipc::DaemonMetrics>,
    health: crate::usecase::application::daemon_health::DaemonHealthTracker,
    git_diffs: &BTreeMap<SessionId, GitDiff>,
    terminal_view: Option<TerminalViewProjection>,
    create_pending: Option<&str>,
) -> Vec<String> {
    render_home_material(&home_frame_material(
        height,
        width,
        runtime,
        workspace_name,
        sessions,
        metrics,
        health,
        git_diffs,
        terminal_view,
        create_pending,
        Utc::now(),
    ))
}

// 1 つの決定表を分けると読み手が追う状態が増えるため、この関数はまとめて置く。
#[allow(clippy::too_many_lines)]
/// Apply actions already routed by [`DaemonBackend`] to the stateful terminal
/// host. This layer owns no Effect matching and therefore cannot diverge from
/// the backend's route matrix.
pub(super) fn drain_controller_host_actions(
    actions: &Receiver<ControllerHostAction>,
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    pending_targets: &mut std::collections::HashMap<OperationId, Target>,
    session_refresh: &mut dyn SessionRefreshPort,
    pending_session_refresh: &mut Option<Completions>,
) {
    for action in actions.try_iter().take(FRAME_EVENT_BUDGET) {
        match action {
            ControllerHostAction::Create(request, completions) => {
                let name = request.intent.name;
                let base_ref = request.intent.base_ref;
                let role_id = request.intent.role_id;
                let before = ui.workspace.session_ids().to_vec();
                if begin_session_command(
                    ui,
                    SessionCommand::Create {
                        name: name.clone(),
                        role_id,
                        base_ref,
                    },
                    SessionBackendCompletion::Create {
                        token: request.token,
                        before,
                        completions,
                    },
                ) {
                    ui.creating_session = Some(PendingCreate { name });
                }
            }
            ControllerHostAction::Refresh(_, completions) => {
                // A refresh is an observation, not a command: it goes to the
                // resident lane instead of spawning a worker with its own
                // daemon connection (#551). Several requests inside one cadence
                // period coalesce onto the snapshot that lane publishes next.
                session_refresh.wake();
                *pending_session_refresh = Some(completions);
            }
            ControllerHostAction::Remove(request, completions) => {
                if let Some(name) = session_name_for(ui, request.session) {
                    let before = ui.workspace.session_ids().to_vec();
                    if begin_session_command(
                        ui,
                        SessionCommand::Remove {
                            name,
                            force: request.force,
                            force_delete_branch: request.force_delete_branch,
                            purge_orphan: request.purge_orphan,
                        },
                        SessionBackendCompletion::Remove {
                            session: request.session,
                            before,
                            completions,
                        },
                    ) {
                        ui.removing_session = Some(request.session);
                    }
                } else {
                    completions.emit(AppEvent::Backend(BackendEvent::Notice(Notice::new(
                        "selected session is no longer available",
                    ))));
                }
            }
            ControllerHostAction::Sleep(request, completions) => {
                if let Some(name) = session_name_for(ui, request.session) {
                    let before = ui.workspace.session_ids().to_vec();
                    begin_session_command(
                        ui,
                        SessionCommand::Sleep { name },
                        SessionBackendCompletion::Sleep {
                            before,
                            completions,
                        },
                    );
                } else {
                    completions.emit(AppEvent::Backend(BackendEvent::Notice(Notice::new(
                        "selected session is no longer available",
                    ))));
                }
            }
            ControllerHostAction::LaunchAgent(request) => {
                let target = request
                    .session
                    .map_or(Target::Root(request.workspace), Target::Session);
                pending_targets.insert(request.operation_id, target);
                if let Some(goal) = &request.goal {
                    runtime.on_effect(&Effect::LaunchGoal {
                        workspace: request.workspace,
                        operation_id: request.operation_id,
                        profile: request.profile.clone(),
                        goal: goal.clone(),
                    });
                } else {
                    runtime.on_effect(&Effect::LaunchAgent {
                        workspace: request.workspace,
                        session: request.session,
                        operation_id: request.operation_id,
                        profile: request.profile.clone(),
                    });
                }
                enqueue_pane_launch(
                    ui,
                    PaneLaunch::Agent {
                        operation: request.operation_id,
                        workspace: request.workspace,
                        session: request.session,
                        profile: request.profile,
                        goal: request.goal,
                        resume: false,
                    },
                );
            }
            ControllerHostAction::ResumeAgent(request) => {
                let target = Target::Session(request.session);
                pending_targets.insert(request.operation_id, target);
                runtime.on_effect(&Effect::LaunchAgent {
                    workspace: request.workspace,
                    session: Some(request.session),
                    operation_id: request.operation_id,
                    profile: None,
                });
                enqueue_pane_launch(
                    ui,
                    PaneLaunch::Agent {
                        operation: request.operation_id,
                        workspace: request.workspace,
                        session: Some(request.session),
                        profile: None,
                        goal: None,
                        resume: true,
                    },
                );
            }
            ControllerHostAction::ReopenAgent(request) => {
                if ui
                    .agent
                    .as_ref()
                    .is_some_and(|agent| request.workspace == agent.workspace)
                {
                    let reopened = ui.mutate_agent_intent(AgentTabIntentMutation::Reopen {
                        continuation: request.continuation,
                    });
                    match reopened {
                        Ok(()) => {
                            ui.request_agent_observation();
                            let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::Notice(
                                Notice::new(
                                    "Agent reopen was saved; waiting for daemon observation",
                                ),
                            )));
                        }
                        Err(error) => surface_agent_tab_intent_error(runtime, error),
                    }
                }
            }
            ControllerHostAction::OpenTerminal(request) => {
                if let Some(agent) = ui.agent.as_ref() {
                    let workspace = agent.workspace;
                    pending_targets.insert(request.operation_id, request.target);
                    runtime.on_effect(&Effect::OpenTerminal {
                        target: request.target,
                        operation_id: request.operation_id,
                        arguments: request.arguments.clone(),
                    });
                    enqueue_pane_launch(
                        ui,
                        PaneLaunch::Terminal {
                            operation: request.operation_id,
                            workspace,
                            session: request.target.session_id(),
                            arguments: request.arguments,
                        },
                    );
                } else {
                    runtime.on_effect(&Effect::OpenTerminal {
                        target: request.target,
                        operation_id: request.operation_id,
                        arguments: request.arguments,
                    });
                    fail_terminal_launch(
                        runtime,
                        request.target,
                        request.operation_id,
                        "terminal launch is unavailable".to_owned(),
                    );
                }
            }
            ControllerHostAction::OpenExternalTerminal(target) => {
                let path = match target {
                    Target::Root(_) => Some(ui.workspace.path().to_path_buf()),
                    Target::Session(session) => ui
                        .workspace
                        .sessions()
                        .iter()
                        .zip(ui.workspace.session_ids())
                        .find(|(_, id)| **id == session)
                        .map(|(record, _)| record.root.clone()),
                };
                match path {
                    Some(path) => {
                        if let Err(error) = ui.external_terminal.open(&path) {
                            let _ = runtime
                                .apply_event(AppEvent::TerminalLaunchFailed(Notice::new(error)));
                        }
                    }
                    None => {
                        let _ = runtime.apply_event(AppEvent::TerminalLaunchFailed(Notice::new(
                            "selected session is no longer available",
                        )));
                    }
                }
            }
            ControllerHostAction::SelectTab(direction) => {
                let Some(active) = runtime.panes().active() else {
                    continue;
                };
                let Some(selection) = runtime.selection_after_select(direction) else {
                    continue;
                };
                if !ui.has_agent_intent_for(active.session_id()) {
                    runtime.on_effect(&Effect::SelectTab { direction });
                    activate_focused_interrupted_tab(ui, runtime, pending_targets);
                    continue;
                }
                let continuation = match &selection {
                    TabSelection::Live(terminal) => ui.agent_continuation_for(terminal),
                    TabSelection::Interrupted(continuation) => Some(*continuation),
                    TabSelection::Pending(_) | TabSelection::Ready(_) => None,
                };
                match ui.mutate_agent_intent(AgentTabIntentMutation::Select {
                    session_id: active.session_id(),
                    continuation,
                }) {
                    Ok(()) => {
                        runtime.on_effect(&Effect::SelectTab { direction });
                        activate_focused_interrupted_tab(ui, runtime, pending_targets);
                    }
                    Err(error) => surface_agent_tab_intent_error(runtime, error),
                }
            }
        }
    }
}

// 1 つの決定表を分けると読み手が追う状態が増えるため、この関数はまとめて置く。
#[allow(clippy::too_many_lines)]
// 注入された port をそのまま受け取る composition 境界で、束ねると呼び手が構造体を組むだけになる。
#[allow(clippy::too_many_arguments)]
/// Controller-driven real-terminal frame loop (`drain → poll → render → input →
/// dispatch`). Home row state, live-pane availability, and the Home frame come
/// from [`WorkspaceRuntime`]/`render_home`; [`WorkspaceIoRuntime`] holds only
/// daemon transport coordination (session workers, pane launches, terminal
/// streams, metrics) and owns no route or selection state.
#[coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=screen_graph_production_port_harness
pub(super) fn drive_workspace_controller(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    deck: &mut WorkspaceDeck,
    registry: &[Workspace],
    mut loader: Option<&mut dyn WorkspaceLoader>,
    backend_factory: &mut dyn ControllerBackendFactory,
    modal_selection_mode: usagi_core::domain::settings::ModalSelectionMode,
    pr_auto_open: usagi_core::domain::settings::PrAutoOpen,
    entry_policy: WorkspaceEntryPolicy,
    mut workspace_config: Option<WorkspaceConfigContext<'_>>,
) -> io::Result<WorkspaceStep> {
    let mut registry = registry.to_vec();
    let WorkspaceEntryPolicy {
        available_models,
        default_model,
        default_branch,
        work_mode,
        icon_mode,
    } = entry_policy;
    deck.set_icon_mode(icon_mode);
    let workspace_id = snapshot.workspace_id;
    let session_ids = snapshot.session_ids.clone();
    let workspace_name = snapshot.workspace.name.clone();
    let root_cwd = snapshot.workspace.path.clone();
    let agent_resumes = snapshot.agent_resumes.clone();
    let session_lifecycles = snapshot.session_lifecycles.clone();
    let garden_reduced_motion = backend_factory.garden_reduced_motion();
    let (host, host_rx) = ControllerHost::channel();
    let composition = backend_factory.create(&snapshot, host);
    let session_catalogs = composition.session_catalogs;
    let mut backend = composition.backend;
    let mut browser = composition.browser;
    let mut restore_commands = Some(composition.restore_commands);
    let mut restore_connection = composition.restore_connection;
    // Cross-project Garden observation. Its dedicated port is parked here while
    // no round is in flight, exactly like the restore lane's.
    let mut garden_inventory = Some(composition.garden_inventory);
    let mut work_run_port = Some(composition.work_runs);
    // Resident session-inventory lane. The frame loop only wakes and drains it;
    // the observation itself never runs here (#551).
    let mut session_refresh = composition.session_refresh;
    // The `Effect::RefreshSessions` completion parked until the resident lane
    // publishes its next snapshot. Requests inside one cadence period coalesce
    // onto that one snapshot instead of each issuing a request, and every
    // completion sink of a workspace is a clone of the same channel, so keeping
    // the newest is keeping all of them.
    let mut pending_session_refresh: Option<Completions> = None;
    let (restore_sender, restore_completions) = mpsc::channel();
    let (garden_sender, garden_completions) = mpsc::channel::<GardenObservationCompletion>();
    let (work_run_sender, work_run_completions) = mpsc::channel::<WorkRunLaneCompletion>();
    let mut workspace =
        WorkspaceView::with_runtime_ids(snapshot.workspace, snapshot.state, session_ids.clone());
    workspace.set_session_lifecycles(session_lifecycles);
    let mut ui = WorkspaceIoRuntime::new(workspace, composition.session_commands)
        .with_agent_resumes(agent_resumes)
        .with_agent_context(
            workspace_id,
            session_ids.clone(),
            composition.agent_commands,
        )
        .with_pane_launch_port(composition.pane_launch_commands)
        .with_agent_tab_intent(
            workspace_id,
            session_ids.iter().copied().collect(),
            composition.agent_tab_intents,
        )
        .with_external_terminal(composition.external_terminal);
    let mut runtime =
        WorkspaceRuntime::with_selection_mode(workspace_id, session_ids, modal_selection_mode);
    restore_workspace_session_focus(deck, &root_cwd, &mut runtime);
    let mut pending_garden_visit = deck.take_garden_visit(&root_cwd);
    let mut pending_garden_agent = None;
    runtime.set_pr_auto_open(pr_auto_open);
    let role_catalog = session_catalogs.roles(&root_cwd);
    let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::SessionRoleCatalog(
        role_catalog,
    )));
    // Git ref discovery can be slow on large or remote filesystems. Start Home
    // immediately with the daemon's HEAD default, then reflux the catalog from a
    // one-shot worker without ever holding the render thread.
    let (branch_catalog_sender, branch_catalog_receiver) = mpsc::channel();
    let branch_catalog_root = root_cwd.clone();
    let branch_catalog_default = default_branch;
    let branch_catalogs = session_catalogs.branch_worker();
    let _ = std::thread::Builder::new()
        .name("tui-branch-catalog".to_owned())
        .spawn(move || {
            let _ = branch_catalog_sender.send(
                branch_catalogs.branches(&branch_catalog_root, branch_catalog_default.as_deref()),
            );
        });
    runtime.set_agent_models(available_models, default_model);
    runtime.set_work_mode(work_mode);
    if let Some(error) = ui.take_agent_tab_intent_load_error() {
        surface_agent_tab_intent_error(&mut runtime, error);
    }
    let mut metrics_backend = MetricsBackend::new(composition.metrics);
    let mut metrics_projection = MetricsProjection::default();
    let mut pending_targets: std::collections::HashMap<OperationId, Target> =
        std::collections::HashMap::new();
    // The reducer hit-tests sidebar clicks and owns stable-identity double-click
    // state. The shell's clock is reduced to a deterministic elapsed timestamp.
    let pointer_clock = std::time::Instant::now();
    // The screen saver's idle deadline rides the same clock: the shell observes
    // user input and monotonic time, the reducer only sees an injected duration.
    let mut idle_watch = IdleWatch::new(pointer_clock.elapsed());
    // A Garden mouse-down owns the complete drag/release gesture even though
    // the down itself closes the overlay. Later gesture phases must not land on
    // the newly revealed terminal or sidebar.
    let mut garden_pointer_gesture = false;
    // Live-terminal scroll offset, drag selection, and copy feedback the reducer
    // does not own (design §4.2).
    let mut controls = LiveTerminalControls::default();
    // Seed the daemon-authoritative snapshots before the first frame so a
    // pending decision and another client's sessions are visible without
    // requiring a manual key binding. Both are wakes of a resident lane, not
    // synchronous requests: the frame loop issues no daemon RPC of its own
    // (#551).
    let _ = backend.dispatch(Effect::RefreshDecisions {
        workspace: workspace_id,
    });
    let _ = backend.dispatch(Effect::SyncPullRequestTargets {
        sessions: runtime.state().sessions().to_vec(),
    });
    session_refresh.wake();
    // Start restore after the first frame. The controller owns retry admission
    // and a capped backoff across worker jobs; a frame tick never resets it.
    let restore_clock = std::time::Instant::now();
    let mut restore_retry = RestoreRetryState::new();
    let mut registry_refresh_pending = false;
    let mut registry_refresh_due = std::time::Duration::ZERO;
    let mut garden_observation =
        ObservationLane::new(GARDEN_OBSERVATION_INTERVAL, GARDEN_OBSERVATION_BACKOFF);
    let mut garden_observations = 0_u64;
    let mut work_run_observation =
        ObservationLane::new(WORK_RUN_OBSERVATION_INTERVAL, WORK_RUN_OBSERVATION_BACKOFF);
    let mut work_runs = WorkRunProjection::default();
    let mut work_run_control = WorkRunControl::default();
    let mut help_context: Option<super::views::key_help::State> = None;
    let mut pending_work_run_control = None;
    let mut work_run_revision = 0_u64;
    // Filesystem hint for the inline create form. It is off the frame budget:
    // no scan happens while the form is closed (#554).
    let mut worktree_hint = SessionWorktreeHint::new(composition.session_worktrees);
    // Material of the frame currently on screen. A tick whose material matches
    // it draws nothing: the frame build and the terminal diff are both skipped.
    // Everything else in this loop — drains, admission, input — runs regardless.
    let mut drawn_material: Option<HomeFrameMaterial> = None;
    // Owned daemon row/path material is rebuilt only when its authoritative
    // inputs change. The cache never feeds commands back into the controller.
    let mut session_material_key: Option<(u64, Option<SessionId>, u64)> = None;
    let mut sessions: Arc<[ProjectedSession]> = Arc::from([]);
    let mut metrics_sessions = Vec::new();
    let mut terminal_material_key: Option<(Option<TerminalRef>, u64, u64, Geometry)> = None;
    let mut terminal_view: Option<Arc<TerminalViewProjection>> = None;
    let mut terminal_rows_len = 0;
    let mut terminal_scroll = 0;
    let mut terminal_generation = 0_u64;
    let mut background_terminal_material_key = None;
    let mut background_terminal_view: Option<Arc<TerminalViewProjection>> = None;
    let mut background_terminal_generation = 0_u64;
    let mut director_terminal_material_key = None;
    let mut director_terminal_view: Option<Arc<TerminalViewProjection>> = None;
    let mut director_terminal_generation = 0_u64;
    let mut root_terminal_material_key = None;
    let mut root_terminal_view: Option<Arc<TerminalViewProjection>> = None;
    let mut root_terminal_generation = 0_u64;
    let mut director_material_key = None;
    // The source key remembers the raw Garden cadence so a held pose is
    // canonicalized once. The material key stores only the canonical visible
    // tick and therefore names the frame actually sent to the terminal.
    let mut frame_source_key: Option<FrameMaterialKey> = None;
    let mut frame_material_key: Option<FrameMaterialKey> = None;
    let mut allowed_sessions_revision = u64::MAX;
    let mut current_sessions = BTreeSet::new();
    loop {
        let registry_now = restore_clock.elapsed();
        if deck.add_overlay_open()
            && !registry_refresh_pending
            && registry_now >= registry_refresh_due
            && let Some(loader) = loader.as_mut()
        {
            match (**loader).dispatch_registry_refresh() {
                Ok(true) => registry_refresh_pending = true,
                Ok(false) => {
                    registry_refresh_due = registry_now + REGISTRY_REFRESH_INTERVAL;
                }
                Err(error) => {
                    deck.set_notice(error.to_string());
                    registry_refresh_due = registry_now + REGISTRY_REFRESH_INTERVAL;
                    drawn_material = None;
                    frame_source_key = None;
                    frame_material_key = None;
                }
            }
        }
        if let Some(completion) = loader
            .as_mut()
            .and_then(|loader| (**loader).take_registry_refresh())
        {
            registry_refresh_pending = false;
            registry_refresh_due = registry_now + REGISTRY_REFRESH_INTERVAL;
            match completion {
                Ok(latest) => {
                    registry = latest;
                    if deck.add_overlay_open() {
                        deck.refresh_add(&registry);
                        drawn_material = None;
                        frame_source_key = None;
                        frame_material_key = None;
                    }
                }
                Err(error) if deck.add_overlay_open() => {
                    deck.set_notice(error.to_string());
                    drawn_material = None;
                    frame_source_key = None;
                    frame_material_key = None;
                }
                Err(_) => {}
            }
        }
        if let Ok(catalog) = branch_catalog_receiver.try_recv() {
            let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::SessionBranchCatalog(
                catalog,
            )));
        }
        for event in backend.drain_events_bounded(FRAME_EVENT_BUDGET) {
            let _ = runtime.apply_event(event);
        }
        while let Some(epoch) = restore_connection.take_reconnected_epoch() {
            restore_retry.reconnected(epoch, restore_clock.elapsed());
        }
        drain_controller_host_actions(
            &host_rx,
            &mut ui,
            &mut runtime,
            &mut pending_targets,
            session_refresh.as_mut(),
            &mut pending_session_refresh,
        );
        if ui.take_agent_observation_request() {
            restore_retry.request_observation(restore_clock.elapsed());
        }
        if ui.take_agent_inventory_change_observation_request() {
            restore_retry.request_changed_observation(restore_clock.elapsed());
        }
        drain_session_completions(&mut ui);
        drain_session_refresh(
            &mut ui,
            session_refresh.as_mut(),
            &mut pending_session_refresh,
        );
        let worktree_names = worktree_hint.names(
            runtime.state().create_session_form().is_some(),
            ui.workspace.path(),
            restore_clock.elapsed(),
        );
        sync_runtime_sessions(&mut runtime, &ui, worktree_names);
        restore_workspace_closeup(deck, &root_cwd, &mut runtime);
        if let Some(visit) = pending_garden_visit.take() {
            let _ = runtime.apply_event(AppEvent::VisitSession(visit.session));
            pending_garden_agent = visit.agent.map(|agent| (visit.session, agent));
        }
        let workspace_material_revision = ui.workspace.material_revision();
        if allowed_sessions_revision != workspace_material_revision {
            current_sessions = ui.workspace.session_ids().iter().copied().collect();
            ui.set_allowed_agent_sessions(current_sessions.iter().copied());
            allowed_sessions_revision = workspace_material_revision;
        }
        for completion in restore_completions.try_iter().take(FRAME_EVENT_BUDGET) {
            let applied = apply_restore_completion(
                completion,
                &mut ui,
                &mut runtime,
                workspace_id,
                &current_sessions,
            );
            let outcome = applied.outcome;
            let show_notice = restore_retry.complete(restore_clock.elapsed(), outcome);
            restore_commands = Some(applied.port);
            if show_notice {
                let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::Notice(Notice::new(
                    "daemon restore is unavailable after retries; no Agent was started",
                ))));
            }
            if let RestoreJobOutcome::IntentFailed(error) = outcome {
                surface_agent_tab_intent_error(&mut runtime, error);
            }
        }
        if let Some((session, agent)) = pending_garden_agent {
            let selected = visit_garden_agent(
                &mut ui,
                &mut runtime,
                GardenClick::Visit {
                    workspace: workspace_id,
                    session,
                    agent: Some(agent),
                },
            );
            if selected {
                activate_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending_targets);
            }
            if selected || ui.agent_inventory().is_some() {
                pending_garden_agent = None;
            }
        }
        for completion in garden_completions.try_iter().take(FRAME_EVENT_BUDGET) {
            let observed = !completion.inventories.is_empty();
            let mut changed = false;
            for inventory in &completion.inventories {
                changed |= deck.apply_garden_inventory(inventory);
            }
            if changed {
                garden_observations = garden_observations.wrapping_add(1);
            }
            garden_inventory = Some(completion.port);
            garden_observation.complete(restore_clock.elapsed(), observed);
        }
        for completion in work_run_completions.try_iter().take(FRAME_EVENT_BUDGET) {
            match completion {
                WorkRunLaneCompletion::Observation { port, snapshot } => {
                    let observed = snapshot.is_ok();
                    let next = match snapshot {
                        Ok(snapshot) => WorkRunProjection::fresh(snapshot.runs),
                        Err(_) => work_runs.clone().unavailable(),
                    };
                    if next != work_runs {
                        work_runs = next;
                        work_run_control.sync_selection(work_runs.runs());
                        work_run_revision = work_run_revision.wrapping_add(1);
                    }
                    work_run_port = Some(port);
                    work_run_observation.complete(restore_clock.elapsed(), observed);
                }
                WorkRunLaneCompletion::Control {
                    port,
                    operation_id,
                    result,
                } => {
                    let result = *result;
                    let previous_control = work_run_control.clone();
                    let accepted = work_run_control.complete(operation_id, &result);
                    if accepted && let Ok(result) = &result {
                        match result {
                            WorkRunControlResult::Updated(run) => {
                                work_runs.apply_control(run.as_ref().clone());
                            }
                            WorkRunControlResult::Deleted(deletion) => {
                                work_runs.apply_deletion(*deletion);
                                work_run_control.sync_selection(work_runs.runs());
                            }
                        }
                    }
                    if work_run_control != previous_control || accepted {
                        work_run_revision = work_run_revision.wrapping_add(1);
                    }
                    work_run_port = Some(port);
                    work_run_observation.refresh_now();
                }
            }
        }
        let (terminal_height, width) = term.size()?;
        let height = terminal_height.saturating_sub(PROJECT_BAR_ROWS);
        ui.set_terminal_size(height, width);
        let _ = runtime.apply_event(AppEvent::Resize {
            width: u16::try_from(width).unwrap_or(u16::MAX),
            height: u16::try_from(height).unwrap_or(u16::MAX),
        });
        let garden_available = garden_fits(height, width);
        if runtime.state().overlay() == Some(Overlay::Garden) && !garden_available {
            let _ = runtime.apply_event(AppEvent::GardenUnavailable);
        }
        let _ = runtime.apply_event(AppEvent::GardenAvailability(garden_available));
        let geometry = foreground_terminal_geometry(
            height,
            width,
            runtime.state().director_drawer_open(),
            runtime.state().root_terminal_drawer_open(),
            runtime.state().root_terminal_full_height(),
            runtime.state().workspace_drawer_focus(),
        );
        drain_pane_completions_into_runtime(&mut ui, &mut runtime, &mut pending_targets, geometry);
        // Keep every terminal in the Home composition attached. Concurrent
        // Director and root-terminal drawers add two root surfaces above the
        // managed-session Agent instead of replacing, resizing, or detaching it.
        let director_terminal = runtime.director_terminal();
        let root_terminal = runtime.root_terminal();
        let terminal_attachments = workspace_terminal_attachments(&runtime, height, width);
        ui.sync_visible_terminals(&terminal_attachments);
        // Polling still runs every tick so output/admission progresses, but row
        // String creation and URL scanning run only behind the projection key.
        close_exited_panes(&mut ui, &mut runtime);
        sync_terminal_selection_motions(&mut ui, &mut controls);
        let focused_terminal = runtime.preview_terminal();
        let mut live_terminals = runtime.background_terminals();
        if let Some(terminal) = &focused_terminal {
            live_terminals.push(terminal.clone());
        }
        controls.retain_terminals(&live_terminals);
        controls.sync_focus(focused_terminal.as_ref());
        let screen_revision = focused_terminal
            .as_ref()
            .and_then(|terminal| ui.terminal_projection_key(terminal))
            .unwrap_or(0);
        let next_terminal_key = (
            focused_terminal.clone(),
            screen_revision,
            controls.revision(),
            geometry,
        );
        if terminal_material_key.as_ref() != Some(&next_terminal_key) {
            terminal_view =
                controller_terminal_view(&ui, &runtime, &mut controls, usize::from(geometry.rows))
                    .map(Arc::new);
            (terminal_rows_len, terminal_scroll) = match &terminal_view {
                Some(view) => (view.total_rows, controls.scroll()),
                None => (0, 0),
            };
            terminal_material_key = Some(next_terminal_key);
            terminal_generation = terminal_generation.saturating_add(1);
        }
        let background_terminal = managed_background_terminal(&runtime);
        let background_revision = background_terminal
            .as_ref()
            .and_then(|terminal| ui.terminal_projection_key(terminal))
            .unwrap_or(0);
        let background_rows = usize::from(managed_background_terminal_geometry(height, width).rows);
        let next_background_key = (
            background_terminal.clone(),
            background_revision,
            background_rows,
        );
        if background_terminal_material_key.as_ref() != Some(&next_background_key) {
            background_terminal_view = background_terminal
                .as_ref()
                .and_then(|terminal| ui.retained_terminal_view(terminal, background_rows))
                .map(Arc::new);
            background_terminal_material_key = Some(next_background_key);
            background_terminal_generation = background_terminal_generation.saturating_add(1);
        }
        let director_rows = super::director_drawer::terminal_viewport(height, width).rows;
        let director_revision = director_terminal
            .as_ref()
            .and_then(|terminal| ui.terminal_projection_key(terminal))
            .unwrap_or(0);
        let director_focused = director_terminal.as_ref().is_some_and(|director| {
            focused_terminal
                .as_ref()
                .is_some_and(|focused| focused.fences(director))
        });
        let next_director_terminal_key = (
            director_terminal.clone(),
            director_revision,
            director_rows,
            director_focused,
            terminal_generation,
        );
        if director_terminal_material_key.as_ref() != Some(&next_director_terminal_key) {
            director_terminal_view = if director_focused {
                terminal_view.as_ref().map(Arc::clone)
            } else {
                director_terminal
                    .as_ref()
                    .and_then(|terminal| ui.retained_terminal_view(terminal, director_rows))
                    .map(Arc::new)
            };
            director_terminal_material_key = Some(next_director_terminal_key);
            director_terminal_generation = director_terminal_generation.saturating_add(1);
        }
        let root_rows = super::views::root_terminal_drawer::terminal_viewport_for(
            height,
            width,
            super::views::workspace::root_terminal_available_width(
                height,
                width,
                runtime.state().director_drawer_open(),
            ),
        )
        .rows;
        let root_revision = root_terminal
            .as_ref()
            .and_then(|terminal| ui.terminal_projection_key(terminal))
            .unwrap_or(0);
        let root_focused = root_terminal.as_ref().is_some_and(|root| {
            focused_terminal
                .as_ref()
                .is_some_and(|focused| focused.fences(root))
        });
        let next_root_terminal_key = (
            root_terminal.clone(),
            root_revision,
            root_rows,
            root_focused,
            terminal_generation,
        );
        if root_terminal_material_key.as_ref() != Some(&next_root_terminal_key) {
            root_terminal_view = if root_focused {
                terminal_view.as_ref().map(Arc::clone)
            } else {
                root_terminal
                    .as_ref()
                    .and_then(|terminal| ui.retained_terminal_view(terminal, root_rows))
                    .map(Arc::new)
            };
            root_terminal_material_key = Some(next_root_terminal_key);
            root_terminal_generation = root_terminal_generation.saturating_add(1);
        }
        let next_director_key = (
            runtime.material_key(),
            ui.material_revision,
            director_terminal_generation,
            work_run_revision,
        );
        if director_material_key != Some(next_director_key) {
            let drawer_projection =
                director_drawer_projection(&ui, &runtime, director_terminal_view.as_deref())
                    .with_work_runs(work_runs.clone())
                    .with_work_run_control(work_run_control_projection(&work_run_control));
            runtime.set_director_projection(drawer_projection);
            director_material_key = Some(next_director_key);
        }
        if ui.take_terminal_reconnected() {
            let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::Feedback(
                Feedback::Reconnected,
            )));
        }
        let next_session_key = (
            ui.workspace.material_revision(),
            ui.removing_session,
            runtime.state().session_pr_revision(),
        );
        let sessions_changed = session_material_key != Some(next_session_key);
        if sessions_changed {
            sessions = Arc::from(project_controller_sessions(&ui, runtime.state()));
            deck.update_active_sessions(&sessions);
            metrics_sessions = sessions
                .iter()
                .map(|session| (session.id, session.cwd.clone()))
                .collect();
            session_material_key = Some(next_session_key);
        }
        // Reflux daemon metrics / git diffs through the backend drain instead of
        // polling the port inline: the shell folds the updates into its own
        // projection cache, so the material does not ride on the IO runtime.
        metrics_backend.poll(sessions_changed.then_some(metrics_sessions.as_slice()));
        for update in metrics_backend.drain_events() {
            metrics_projection.apply(update);
        }
        let now = relative_time_clock(Utc::now());
        let garden_open = runtime.state().overlay() == Some(Overlay::Garden);
        let drives_tick_animation = ui.creating_session.is_some()
            || sessions.iter().any(|session| session.removing)
            || runtime
                .preview_pane()
                .tabs()
                .iter()
                .any(|tab| matches!(tab, PaneTab::Pending(_)));
        let animation = if garden_reduced_motion {
            0
        } else if garden_open {
            super::widgets::garden::runtime_tick(runtime.state().mascot_tick())
        } else if drives_tick_animation {
            runtime.state().mascot_tick()
        } else {
            super::widgets::mascot::canonical_tick(runtime.state().mascot_tick())
        };
        let next_source_key = FrameMaterialKey {
            height,
            width,
            controller: runtime.material_key(),
            sessions: next_session_key,
            shell: ui.material_revision,
            metrics: metrics_projection.generation(),
            terminal: (
                terminal_generation,
                background_terminal_generation,
                director_terminal_generation,
                root_terminal_generation,
            ),
            animation,
            create_pending: ui
                .creating_session
                .as_ref()
                .map(|create| create.name.clone()),
            garden_observations,
            work_run_revision,
            now,
        };
        if frame_source_key.as_ref() != Some(&next_source_key) {
            let material = home_frame_material_shared(
                height,
                width,
                &runtime,
                &workspace_name,
                Arc::clone(&sessions),
                metrics_projection.metrics(),
                metrics_projection.health(),
                metrics_projection.shared_git_diffs(),
                if runtime.state().workspace_drawer_open() {
                    background_terminal_view.as_ref().map(Arc::clone)
                } else {
                    terminal_view.as_ref().map(Arc::clone)
                },
                root_terminal_view.as_deref(),
                ui.creating_session
                    .as_ref()
                    .map(|create| create.name.as_str()),
                now,
                icon_mode,
            )
            .with_agent_inventory(ui.agent_inventory(), runtime.panes())
            .with_work_runs(work_runs.clone())
            .with_workspace_deck_garden(deck)
            .with_garden_animation(animation, garden_reduced_motion);
            let mut next_frame_key = next_source_key.clone();
            if garden_open {
                next_frame_key.animation = material
                    .projection
                    .garden_animation_tick()
                    .unwrap_or(animation);
            }
            let frame_changed = frame_material_key.as_ref() != Some(&next_frame_key);
            let controller_may_be_noop = frame_material_key
                .as_ref()
                .is_some_and(|previous| previous.differs_only_by_controller(&next_frame_key));
            // Skip only the drawing. A skipped tick has already run every drain
            // above and still runs restore admission, pane launches, and input
            // below, so nothing that makes progress depends on the redraw.
            if frame_changed
                && (!controller_may_be_noop || drawn_material.as_ref() != Some(&material))
            {
                let home = render_home_material(&material);
                let frame = compose_workspace_shell_frame(deck, height, width, &home);
                let frame = match help_context {
                    Some(help) => {
                        super::views::key_help::render_over(terminal_height, width, &frame, help)
                    }
                    None => frame,
                };
                term.draw(&frame)?;
                drawn_material = Some(material);
            }
            frame_source_key = Some(next_source_key);
            frame_material_key = Some(next_frame_key);
        }
        // The other open projects' Agents, observed only while the Garden is the
        // frame. The active project keeps its own controller's richer phases.
        if garden_inventory.is_some()
            && garden_observation.begin_if_due(garden_open, restore_clock.elapsed())
        {
            let targets = deck.observable_workspaces();
            if targets.is_empty() {
                // The only open project is the one this loop already draws.
                garden_observation.complete(restore_clock.elapsed(), true);
            } else {
                let port = garden_inventory
                    .take()
                    .expect("the Garden observation port was checked above");
                spawn_garden_observation_job(port, targets, garden_sender.clone());
            }
        }
        if work_run_port.is_some() && pending_work_run_control.is_some() {
            let port = work_run_port
                .take()
                .expect("the Work Run control port was checked above");
            let request = pending_work_run_control
                .take()
                .expect("the Work Run control request was checked above");
            spawn_work_run_control_job(port, workspace_id, request, work_run_sender.clone());
        } else if work_run_port.is_some()
            && work_run_observation.begin_if_due(true, restore_clock.elapsed())
        {
            let port = work_run_port
                .take()
                .expect("the Work Run observation port was checked above");
            spawn_work_run_observation_job(port, workspace_id, work_run_sender.clone());
        }
        if restore_commands.is_some() && restore_retry.begin_if_due(restore_clock.elapsed()) {
            let port = restore_commands
                .take()
                .expect("restore admission checked the dedicated port");
            let (interaction, registry_revision) = runtime.restore_fence();
            spawn_restore_job(
                port,
                workspace_id,
                current_sessions.clone(),
                interaction,
                registry_revision,
                restore_sender.clone(),
            );
        }
        drain_pane_launches(&mut ui, geometry);
        // Contextual New is retargeted once here so PTY forwarding, pane
        // controls, and the reducer all see one normalized key.
        let raw_key = retarget_drawer_chords(&runtime, term.read_key()?);
        if handle_interrupted_removal_confirmation(&raw_key, &mut ui, &mut runtime) {
            // This prompt is the exclusive frontmost owner. In particular,
            // Ctrl-D and pane-close chords never reach a live tab behind it.
            continue;
        }
        if let Some(help) = help_context.as_mut() {
            if closes_workspace_help(&raw_key, help.context()) {
                help_context = None;
                drawn_material = None;
                frame_source_key = None;
                frame_material_key = None;
            } else if scroll_key_help(help, &raw_key, terminal_height) {
                drawn_material = None;
                frame_source_key = None;
                frame_material_key = None;
            }
            // Help is the exclusive frontmost input owner. Background drains
            // keep running on the next loop, but no command reaches the surface
            // being described.
            continue;
        }
        if opens_workspace_help(&raw_key, deck, &runtime, &work_run_control) {
            help_context = Some(super::views::key_help::State::new(
                workspace_help_context(deck, &runtime, &work_run_control),
                runtime.state().work_mode(),
            ));
            drawn_material = None;
            frame_source_key = None;
            frame_material_key = None;
            continue;
        }
        if dismiss_pr_modal_on_project_bar_click(&mut runtime, &raw_key) {
            continue;
        }
        let bar_click = matches!(raw_key, Key::Click { row: 0, .. });
        let bar_target = match &raw_key {
            Key::Click { column, row: 0 } => project_bar(deck, width)
                .target_at(usize::from(*column))
                .cloned(),
            _ => None,
        };
        let key = adjust_project_bar_pointer(raw_key);

        // Process shortcuts and the remaining project-bar clicks win over the
        // unobscured workspace surface, including a focused live PTY. Foreground
        // overlays/drawers retain ownership through the target resolver below.
        // Plain arrows are deliberately local to unobscured Switch mode.
        let workspace_navigation_target = workspace_navigation_target(deck, runtime.state(), &key);
        let direct_target = match (&key, &bar_target) {
            (Key::Live(LiveTerminalAction::ActivateWorkspace(number)), _) => deck
                .path_at(usize::from(number.saturating_sub(1)))
                .map(Path::to_path_buf),
            (_, Some(ProjectBarTarget::Workspace(path))) => Some(path.clone()),
            _ => workspace_navigation_target,
        };
        let opens_add = matches!(key, Key::Live(LiveTerminalAction::OpenWorkspace))
            || matches!(bar_target, Some(ProjectBarTarget::Add));
        if opens_add {
            deck.open_add(&registry);
            registry_refresh_due = std::time::Duration::ZERO;
            drawn_material = None;
            frame_source_key = None;
            frame_material_key = None;
            continue;
        }
        if matches!(key, Key::Live(LiveTerminalAction::OpenWorkspaceSwitcher)) {
            deck.open_switcher();
            drawn_material = None;
            frame_source_key = None;
            frame_material_key = None;
            continue;
        }
        if bar_click && direct_target.is_none() {
            continue;
        }
        if let Some(path) = direct_target {
            if path == deck.active_path() {
                continue;
            }
            if workspace_has_unsaved_surface(&runtime) {
                deck.open_switcher();
                deck.set_notice("Save or cancel the current draft before switching.");
                drawn_material = None;
                frame_source_key = None;
                frame_material_key = None;
                continue;
            }
            let preserve_closeup = matches!(
                key,
                Key::Live(
                    LiveTerminalAction::PreviousWorkspace | LiveTerminalAction::NextWorkspace
                )
            ) && runtime.state().route() == Route::Home(HomeMode::Closeup);
            if let Some(prepared) =
                prepare_deck_workspace(term, &mut loader, deck, &path, "Opening workspace…")
                && prepare_activation_settings(
                    &mut workspace_config,
                    &mut loader,
                    deck,
                    &root_cwd,
                    &prepared.workspace.path,
                )
            {
                remember_workspace_session_focus(deck, runtime.state());
                if preserve_closeup {
                    deck.schedule_closeup(prepared.workspace.path.clone());
                }
                return Ok(WorkspaceStep::Activate(Box::new(prepared)));
            }
            drawn_material = None;
            frame_source_key = None;
            frame_material_key = None;
            continue;
        }
        if deck.overlay_open() {
            match deck.handle_overlay_key(&key) {
                OverlayIntent::Stay => {}
                OverlayIntent::Cancel => deck.close_overlay(),
                OverlayIntent::Activate(path) => {
                    if path == deck.active_path() {
                        deck.close_overlay();
                    } else if workspace_has_unsaved_surface(&runtime) {
                        deck.set_notice("Save or cancel the current draft before switching.");
                    } else if let Some(prepared) =
                        prepare_deck_workspace(term, &mut loader, deck, &path, "Opening workspace…")
                        && prepare_activation_settings(
                            &mut workspace_config,
                            &mut loader,
                            deck,
                            &root_cwd,
                            &prepared.workspace.path,
                        )
                    {
                        remember_workspace_session_focus(deck, runtime.state());
                        return Ok(WorkspaceStep::Activate(Box::new(prepared)));
                    }
                }
                OverlayIntent::Visit {
                    path,
                    workspace,
                    session,
                } => {
                    if path == deck.active_path() {
                        deck.close_overlay();
                        if workspace == runtime.state().workspace() {
                            let _ = runtime.apply_event(AppEvent::VisitSession(session));
                        }
                    } else if workspace_has_unsaved_surface(&runtime) {
                        deck.set_notice("Save or cancel the current draft before switching.");
                    } else if let Some(prepared) =
                        prepare_deck_workspace(term, &mut loader, deck, &path, "Opening workspace…")
                        && prepare_activation_settings(
                            &mut workspace_config,
                            &mut loader,
                            deck,
                            &root_cwd,
                            &prepared.workspace.path,
                        )
                    {
                        deck.schedule_garden_visit(prepared.workspace.path.clone(), session, None);
                        remember_workspace_session_focus(deck, runtime.state());
                        return Ok(WorkspaceStep::Activate(Box::new(prepared)));
                    }
                }
                OverlayIntent::Add(paths) => {
                    if !paths.is_empty() {
                        if workspace_has_unsaved_surface(&runtime) {
                            deck.set_notice("Save or cancel the current draft before switching.");
                        } else {
                            let current = deck.active_path().to_path_buf();
                            let mut prepared = Vec::with_capacity(paths.len());
                            let mut failed = false;
                            for (index, path) in paths.iter().enumerate() {
                                let label =
                                    format!("Opening workspace {} / {}…", index + 1, paths.len());
                                let Some(snapshot) =
                                    prepare_deck_workspace(term, &mut loader, deck, path, &label)
                                else {
                                    failed = true;
                                    break;
                                };
                                prepared.push(snapshot);
                            }
                            if failed {
                                if let Some(loader) = loader.as_mut() {
                                    let _ = activate_workspace_responsive(
                                        term,
                                        &mut **loader,
                                        &current,
                                        "Restoring current workspace…",
                                    );
                                }
                                prepared.clear();
                            }
                            if let Some(first) = prepared.first().cloned() {
                                if let Some(loader) = loader.as_mut()
                                    && let Err(error) = activate_workspace_responsive(
                                        term,
                                        &mut **loader,
                                        &first.workspace.path,
                                        "Activating workspace…",
                                    )
                                {
                                    deck.set_notice(error.to_string());
                                    drawn_material = None;
                                    frame_source_key = None;
                                    frame_material_key = None;
                                    continue;
                                }
                                if !prepare_batch_settings(
                                    &mut workspace_config,
                                    &mut loader,
                                    deck,
                                    &root_cwd,
                                    &prepared,
                                ) {
                                    drawn_material = None;
                                    frame_source_key = None;
                                    frame_material_key = None;
                                    continue;
                                }
                                // Batch validation leaves the last member selected;
                                // the composition created after this return belongs
                                // to the first newly added tab.
                                if !prepare_activation_settings(
                                    &mut workspace_config,
                                    &mut loader,
                                    deck,
                                    &root_cwd,
                                    &first.workspace.path,
                                ) {
                                    drawn_material = None;
                                    frame_source_key = None;
                                    frame_material_key = None;
                                    continue;
                                }
                                deck.append_snapshots(&prepared);
                                if let Some(loader) = loader.as_mut() {
                                    let _ = (**loader).record_unite(&deck.paths());
                                }
                                remember_workspace_session_focus(deck, runtime.state());
                                return Ok(WorkspaceStep::Activate(Box::new(first)));
                            }
                        }
                    }
                }
                OverlayIntent::Close(path) => {
                    if path == deck.active_path() {
                        let Some(replacement) = deck
                            .replacement_path_after_close(&path)
                            .map(Path::to_path_buf)
                        else {
                            deck.close_path(&path);
                            return Ok(WorkspaceStep::Back);
                        };
                        if workspace_has_unsaved_surface(&runtime) {
                            deck.set_notice("Save or cancel the current draft before closing.");
                        } else if let Some(prepared) = prepare_deck_workspace(
                            term,
                            &mut loader,
                            deck,
                            &replacement,
                            "Opening replacement workspace…",
                        ) && prepare_activation_settings(
                            &mut workspace_config,
                            &mut loader,
                            deck,
                            &root_cwd,
                            &prepared.workspace.path,
                        ) {
                            deck.close_path(&path);
                            remember_workspace_session_focus(deck, runtime.state());
                            return Ok(WorkspaceStep::Activate(Box::new(prepared)));
                        }
                    } else {
                        deck.close_path(&path);
                        if let Some(loader) = loader.as_mut()
                            && let Err(error) = (**loader).record_unite(&deck.paths())
                        {
                            deck.set_notice(error.to_string());
                        }
                    }
                }
            }
            drawn_material = None;
            frame_source_key = None;
            frame_material_key = None;
            continue;
        }
        if garden_pointer_gesture {
            match key {
                Key::Pointer(PointerEvent {
                    kind: PointerKind::Up,
                    ..
                }) => {
                    garden_pointer_gesture = false;
                    continue;
                }
                Key::Pointer(_) => continue,
                _ => garden_pointer_gesture = false,
            }
        }
        // Screen saver admission, before the key is routed anywhere: a wake-up
        // key resets the deadline first, so the frame that wakes the user can
        // never re-open the garden it just closed. A terminal too small to draw
        // a garden emits no idle event at all, leaving its usable Home alone.
        let idle = idle_watch.observe(&key, pointer_clock.elapsed());
        if garden_fits(height, width) {
            let _ = runtime.apply_event(AppEvent::IdleElapsed(idle));
        }
        // The Garden consumes input before the covered pane. List scrolling
        // and clicks need the drawn viewport below; other shell-owned input
        // wakes Home here without reaching its terminal or form.
        if runtime.state().overlay() == Some(Overlay::Garden) {
            if matches!(key, Key::Click { .. }) {
                garden_pointer_gesture = true;
            } else if garden_shell_owned_wake(&key) {
                garden_pointer_gesture = matches!(
                    key,
                    Key::Pointer(PointerEvent {
                        kind: PointerKind::Drag,
                        ..
                    })
                );
                let _ = runtime.apply_event(AppEvent::GardenClick(GardenClick::Dismiss));
                continue;
            }
        }
        // Neither a tick nor a resize refreshes an inventory here any more. Both
        // used to dispatch `RefreshDecisions` + `RefreshSessions`, which ran the
        // daemon round trip on this thread at the 16ms frame cadence; the
        // decision and session lanes are now resident background workers with
        // their own bounded cadence, drained at the head of this loop (#551).
        // A tick and a resize therefore cost exactly one redraw each, and the
        // only wake left is the explicit one a lifecycle action asks for through
        // `ControllerHostAction`.
        let garden_route = route_garden_input(
            &mut ui,
            &mut runtime,
            drawn_material.as_ref(),
            &key,
            &mut garden_pointer_gesture,
        );
        if let Some(GardenInputRoute::Project(visit)) = garden_route {
            let Some(path) = deck
                .path_for_workspace(visit.workspace)
                .map(Path::to_path_buf)
            else {
                deck.open_switcher();
                deck.set_notice("That project is no longer open.");
                garden_pointer_gesture = false;
                drawn_material = None;
                frame_source_key = None;
                frame_material_key = None;
                continue;
            };
            if let Some(prepared) =
                prepare_deck_workspace(term, &mut loader, deck, &path, "Opening workspace…")
                && prepare_activation_settings(
                    &mut workspace_config,
                    &mut loader,
                    deck,
                    &root_cwd,
                    &prepared.workspace.path,
                )
            {
                deck.schedule_garden_visit(
                    prepared.workspace.path.clone(),
                    visit.session,
                    visit.agent,
                );
                remember_workspace_session_focus(deck, runtime.state());
                return Ok(WorkspaceStep::Activate(Box::new(prepared)));
            }
            let notice = deck.notice().map(str::to_owned);
            deck.open_switcher();
            if let Some(notice) = notice {
                deck.set_notice(notice);
            }
            garden_pointer_gesture = false;
            drawn_material = None;
            frame_source_key = None;
            frame_material_key = None;
            continue;
        }
        // The Home header stays above the overlaid Director drawer. Resolve its
        // persistent drawer buttons before the picker gets a chance to consume
        // the click, otherwise an open picker makes the visible buttons inert.
        let pointer_drawer_focus =
            focus_workspace_drawer_from_pointer(&mut runtime, &key, height, width);
        if pointer_drawer_focus.is_some() {
            let focused = runtime.focused_terminal();
            controls.sync_focus(focused.as_ref());
            terminal_rows_len = focused
                .as_ref()
                .and_then(|terminal| ui.terminal_row_extent(terminal, None))
                .map_or(0, |(_, _, total_rows)| total_rows);
            terminal_scroll = controls.scroll();
        }
        let drawer_header_effects = drawn_material.as_ref().and_then(|material| {
            apply_drawer_header_while_director_open(
                &mut runtime,
                &key,
                material.width,
                &material.projection,
            )
        });
        let director_new_effects = drawer_header_effects
            .is_none()
            .then(|| {
                open_director_from_new_button(
                    &mut runtime,
                    &key,
                    height,
                    width,
                    work_run_control.mode(),
                )
            })
            .flatten();
        let director_new_clicked = director_new_effects.is_some();
        if drawer_header_effects.is_some() || director_new_clicked {
            let previous = work_run_control.clone();
            work_run_control.suspend();
            if work_run_control != previous {
                work_run_revision = work_run_revision.wrapping_add(1);
            }
        }
        let work_run_input =
            if garden_route.is_none() && drawer_header_effects.is_none() && !director_new_clicked {
                let previous = work_run_control.clone();
                let input = handle_work_run_control_input_with_ui(
                    Some(&mut ui),
                    &mut runtime,
                    &mut work_run_control,
                    &work_runs,
                    &key,
                );
                if work_run_control != previous {
                    work_run_revision = work_run_revision.wrapping_add(1);
                }
                input
            } else {
                None
            };
        if let Some(WorkRunControlInput {
            outcome: WorkRunControlOutcome::Submit(request),
            ..
        }) = work_run_input.as_ref()
        {
            pending_work_run_control = Some(request.clone());
        }
        let work_run_effects = work_run_input.map(|input| input.effects);
        let input_route = match garden_route {
            Some(GardenInputRoute::Local(effects)) => WorkspaceInputRoute::Garden(effects),
            Some(GardenInputRoute::Agent(effects)) => {
                activate_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending_targets);
                WorkspaceInputRoute::Garden(effects)
            }
            Some(GardenInputRoute::Project(_)) => unreachable!("project visit returned above"),
            None if drawer_header_effects.is_some() => {
                WorkspaceInputRoute::Drawer(drawer_header_effects.expect("matched above"))
            }
            None if director_new_clicked => WorkspaceInputRoute::Drawer(
                director_new_effects.expect("matched Director New button"),
            ),
            None if work_run_effects.is_some() => WorkspaceInputRoute::Drawer(
                work_run_effects.expect("matched Work Run control input"),
            ),
            None => route_workspace_input_before_reducer(
                &mut ui,
                &mut runtime,
                &mut controls,
                term,
                &key,
            ),
        };
        if input_route == WorkspaceInputRoute::Forwarded {
            continue;
        }
        // Live-terminal view controls the reducer does not own (scroll, tab close,
        // pointer drag / copy — design §4.2) are handled before the key reaches
        // the Home reducer.
        if input_route == WorkspaceInputRoute::Unhandled
            && intercept_live_terminal_control(
                &key,
                &mut ui,
                &mut runtime,
                &mut controls,
                term,
                browser.as_mut(),
                &mut pending_targets,
                height,
                width,
                terminal_rows_len,
                terminal_scroll,
            )
        {
            continue;
        }
        if pointer_drawer_focus.is_some()
            && matches!(key, Key::Click { .. })
            && !director_new_clicked
        {
            // Drawer chrome owns its whole rectangle. A click outside the PTY
            // viewport must not activate the Home sidebar hidden underneath.
            continue;
        }
        let daemon_overlay_was_open = runtime.state().overlay() == Some(Overlay::Daemon);
        let effects = if let WorkspaceInputRoute::Drawer(effects)
        | WorkspaceInputRoute::Garden(effects) = input_route
        {
            effects
        } else if let Key::Click { column, row } = key {
            if let Some(route) =
                route_pr_modal_click(runtime.state().overlay(), height, width, column, row)
            {
                if route == PrModalClickRoute::Inside {
                    // The modal owns its whole box; a click there must not
                    // activate a header, pane tab, or sidebar row behind it.
                    Vec::new()
                } else {
                    runtime.apply_event(AppEvent::Key(AppKey::Escape))
                }
            } else {
                // Header rendering and hit-testing share one layout projection, so
                // Notice presence and narrow clipping cannot move an action away
                // from its clickable cells.
                let header_action = drawn_material.as_ref().and_then(|material| {
                    home_header_action_at(width, &material.projection, column, row)
                });
                let pane_tab = runtime
                    .wants_right_pane_tab_click()
                    .then(|| {
                        drawn_material.as_ref().and_then(|material| {
                            right_pane_tab_at(
                                material.height,
                                material.width,
                                &material.projection,
                                column,
                                row,
                            )
                        })
                    })
                    .flatten();
                match (header_action, pane_tab) {
                    (Some(HomeHeaderAction::Director), _) => {
                        runtime.apply_event(AppEvent::Key(AppKey::ToggleDirectorDrawer))
                    }
                    (Some(HomeHeaderAction::RootTerminal), _) => {
                        runtime.apply_event(AppEvent::Key(AppKey::ToggleRootTerminalDrawer))
                    }
                    (Some(HomeHeaderAction::Decisions), _) => {
                        runtime.apply_event(AppEvent::Key(AppKey::OpenDecisions))
                    }
                    (None, Some(index)) => {
                        if select_right_pane_tab(&mut ui, &mut runtime, index) {
                            activate_focused_interrupted_tab(
                                &mut ui,
                                &mut runtime,
                                &mut pending_targets,
                            );
                        }
                        Vec::new()
                    }
                    (None, None) => runtime.apply_event(sidebar_pointer_event(
                        column,
                        row,
                        pointer_clock.elapsed(),
                    )),
                }
            }
        } else {
            runtime.handle_key(key)
        };
        if !daemon_overlay_was_open && runtime.state().overlay() == Some(Overlay::Daemon) {
            ui.refresh_agent_inventory();
        }
        for effect in effects {
            let opens_workspace_config = matches!(
                &effect,
                Effect::WorkspaceCommand {
                    workspace,
                    command: crate::usecase::overview::Command::Config { arguments },
                } if *workspace == workspace_id && arguments.trim().is_empty()
            );
            if opens_workspace_config && let Some(context) = workspace_config.as_mut() {
                // Rebuild after the reducer closed the Overview overlay. The
                // terminal projection is cloned only on this rare Config path;
                // the ordinary frame moved it into `HomeFrameMaterial`.
                let terminal_view = drawn_material
                    .as_ref()
                    .and_then(|material| material.projection.terminal_view().cloned());
                let home = render_controller_frame(
                    height,
                    width,
                    &runtime,
                    &workspace_name,
                    &sessions,
                    metrics_projection.metrics(),
                    metrics_projection.health(),
                    metrics_projection.git_diffs(),
                    terminal_view,
                    ui.creating_session
                        .as_ref()
                        .map(|create| create.name.as_str()),
                );
                // Workspace frames reserve row zero for the project bar. Keep
                // that exact composition behind Config; otherwise the Home
                // projection is drawn one row too high while the modal is open.
                let base = compose_workspace_shell_frame(deck, height, width, &home);
                let branch_catalog = session_catalogs.branches(
                    &root_cwd,
                    usagi_core::usecase::settings::read_for_workspace_entry(context.settings)
                        .default_branch
                        .as_deref(),
                );
                run_workspace_config(
                    term,
                    context.settings,
                    context.available_models,
                    &branch_catalog.branches,
                    &base,
                )?;
                // The modal drew over the frame the gate remembers, so the next
                // tick must redraw even if no material changed underneath it.
                drawn_material = None;
                frame_source_key = None;
                frame_material_key = None;
                let effective =
                    usagi_core::usecase::settings::read_for_workspace_entry(context.settings);
                runtime.set_modal_selection_mode(effective.modal_selection_mode);
                runtime.set_pr_auto_open(effective.pr_auto_open);
                // A newly saved Agent default applies to the next `agent`
                // command without reopening the workspace.
                runtime.set_agent_models(context.available_models, effective.default_model);
                runtime.set_work_mode(effective.work_mode);
                let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::SessionBranchCatalog(
                    session_catalogs.branches(&root_cwd, effective.default_branch.as_deref()),
                )));
                // Team selection changes the effective role catalog immediately
                // for the next session creation or Agent launch.
                let role_catalog = session_catalogs.roles(&root_cwd);
                let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::SessionRoleCatalog(
                    role_catalog,
                )));
                continue;
            }
            // Both stops return from here, which is what performs the teardown:
            // every port, pump, worker, and live-terminal subscription this
            // workspace established is owned by this frame, so returning drops
            // them before the caller can open another workspace (#556).
            match backend.dispatch(effect) {
                BackendFlow::Continue => {}
                BackendFlow::Exit => return Ok(WorkspaceStep::Quit),
                BackendFlow::Leave => return Ok(WorkspaceStep::Back),
            }
        }
    }
}

/// Everything an entry screen's frame is a function of.
///
/// The entry screens have no clock and no background lane: each `render_*` is
/// pure in the terminal size and the form on screen, so comparing this value
/// against the last drawn one is an exact redraw test (#554). The `now` the
/// renderers receive is fixed for the whole run and therefore not material.
///
/// Holding the form by value means cloning it once per tick. That is a handful
/// of short strings and paths, kept deliberately in exchange for the full
/// screen build, ANSI parse and cell diff it lets an idle tick skip.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct EntryFrameMaterial {
    pub(super) height: usize,
    pub(super) width: usize,
    pub(super) form: EntryForm,
    pub(super) missing_workspace: Option<MissingWorkspacePrompt>,
}

impl EntryFrameMaterial {
    fn new(
        height: usize,
        width: usize,
        screen: Screen,
        welcome: &Welcome,
        open: &Open,
        new_form: &New,
        config_form: &Config,
    ) -> Self {
        let form = match screen {
            Screen::Welcome => EntryForm::Welcome(welcome.clone()),
            Screen::Open => EntryForm::Open(open.clone()),
            Screen::New => EntryForm::New(new_form.clone()),
            Screen::Config => EntryForm::Config(config_form.clone()),
        };
        Self {
            height,
            width,
            form,
            missing_workspace: None,
        }
    }

    fn with_missing_workspace(mut self, prompt: Option<&MissingWorkspacePrompt>) -> Self {
        self.missing_workspace = prompt.cloned();
        self
    }

    fn render(&self, now: DateTime<Utc>) -> Vec<String> {
        let base = match &self.form {
            EntryForm::Welcome(welcome) => {
                super::views::welcome::render(self.height, self.width, welcome, now)
            }
            EntryForm::Open(open) => render_open(self.height, self.width, open, now),
            EntryForm::New(form) => super::views::new::render(self.height, self.width, form),
            EntryForm::Config(form) => super::views::config::render(self.height, self.width, form),
        };
        match self.missing_workspace.as_ref() {
            Some(prompt) => render_missing_workspace_prompt(self.height, self.width, &base, prompt),
            None => base,
        }
    }
}

// 1 つの決定表を分けると読み手が追う状態が増えるため、この関数はまとめて置く。
#[allow(clippy::too_many_lines)]
// 注入された port をそのまま受け取る composition 境界で、束ねると呼び手が構造体を組むだけになる。
#[allow(clippy::too_many_arguments)]
/// [`run_screen_graph_with_backend`] that opens with `notice` already on the
/// Welcome screen.
///
/// This is what makes an entry that could not open its workspace land *inside*
/// the TUI rather than back at the shell: the composition root turns the failure
/// into the same notice the Recent list would have shown
/// ([`open_failure_notice`]), and the switcher comes up with it. The first frame
/// therefore explains why the requested workspace is not on screen, and the user
/// can pick another one or retry the same one.
///
/// # Errors
///
/// Returns workspace loading, settings, or terminal IO failures.
pub(crate) fn run_screen_graph_with_backend_and_notice(
    term: &mut dyn Terminal,
    workspaces: Vec<Workspace>,
    recent: Vec<Recent>,
    now: DateTime<Utc>,
    start: Start,
    loader: &mut dyn WorkspaceLoader,
    settings: &mut dyn SettingsPort,
    backend_factory: &mut dyn ControllerBackendFactory,
    available_models: AvailableAgentModels,
    notice: Option<String>,
) -> io::Result<Exit> {
    let mut registry = workspaces.clone();
    let mut welcome = Welcome::new(recent);
    welcome.set_notice(notice);
    let mut open = open_from_registry(workspaces, welcome.all_recent());
    let mut new_form = New::default();
    let mut config_form = Config::load_with_available_models(settings, available_models);
    let mut screen = match start {
        Start::Welcome => Screen::Welcome,
        Start::Config => Screen::Config,
    };
    // Material of the frame currently on screen. Every entry screen renders a
    // pure function of its size and its form — there is no clock and no
    // background lane here — so a tick that leaves both unchanged draws
    // nothing (#554).
    let mut drawn_material: Option<EntryFrameMaterial> = None;
    let mut help_context: Option<super::views::key_help::State> = None;
    let mut next_create_token = 1_u64;
    let mut pending_create: Option<PendingWorkspaceCreate> = None;
    let mut missing_workspace_prompt: Option<MissingWorkspacePrompt> = None;
    loop {
        let mut created_snapshot = None;
        while let Some(completion) = loader.take_create_completion() {
            let Some(pending) = pending_create.take_if(|pending| {
                pending.token == completion.token && pending.request == completion.request
            }) else {
                continue;
            };
            new_form.finish_create();
            if pending.cancelled {
                let notice = match completion.result {
                    Ok(_) => "creation finished after leaving; workspace was not opened".to_owned(),
                    Err(error) => new_project_notice(&error),
                };
                new_form.set_notice(Some(notice));
                continue;
            }
            match completion.result {
                Ok(snapshot) => {
                    new_form.set_notice(None);
                    created_snapshot = Some(snapshot);
                }
                Err(error) => new_form.set_notice(Some(new_project_notice(&error))),
            }
        }
        if let Some(snapshot) = created_snapshot {
            if !registry_contains_path(&registry, &snapshot.workspace.path) {
                registry.push(snapshot.workspace.clone());
            }
            welcome.record_opened(&snapshot.workspace);
            open.record_opened(&snapshot.workspace);
            if let Some(exit) = enter_workspace(
                term,
                snapshot,
                &registry,
                loader,
                settings,
                backend_factory,
                available_models,
            )? {
                return Ok(exit);
            }
            screen = Screen::Welcome;
            drawn_material = None;
            continue;
        }
        let (height, width) = term.size()?;
        let material = EntryFrameMaterial::new(
            height,
            width,
            screen,
            &welcome,
            &open,
            &new_form,
            &config_form,
        )
        .with_missing_workspace(missing_workspace_prompt.as_ref());
        if drawn_material.as_ref() != Some(&material) {
            let frame = material.render(now);
            let frame = match help_context {
                Some(help) => super::views::key_help::render_over(height, width, &frame, help),
                None => frame,
            };
            term.draw(&frame)?;
            drawn_material = Some(material);
        }
        let key = term.read_key()?;
        if let Some(help) = help_context.as_mut() {
            if matches!(key, Key::Help | Key::Escape) {
                help_context = None;
                drawn_material = None;
            } else if scroll_key_help(help, &key, height) {
                drawn_material = None;
            }
            continue;
        }
        if key == Key::Help {
            help_context = Some(super::views::key_help::State::new(
                entry_help_context(
                    screen,
                    &open,
                    &config_form,
                    missing_workspace_prompt.is_some(),
                ),
                WorkMode::Classic,
            ));
            drawn_material = None;
            continue;
        }
        let explicit_remove = matches!(&key, Key::Char('y' | 'Y'));
        if let Some(prompt) = missing_workspace_prompt.as_mut() {
            match key {
                Key::Left | Key::Right | Key::Tab => {
                    prompt.confirmation.toggle();
                }
                Key::Char('y' | 'Y') | Key::Enter
                    if explicit_remove || prompt.confirmation.is_confirm_selected() =>
                {
                    let paths = prompt.paths.clone();
                    missing_workspace_prompt = None;
                    let candidates = registry
                        .iter()
                        .filter(|workspace| paths.contains(&workspace.path))
                        .cloned()
                        .collect::<Vec<_>>();
                    let removed = loader.cleanup_missing(&candidates)?;
                    open.remove_paths(&removed);
                    welcome.remove_paths(&removed);
                    remove_registry_paths(&mut registry, &removed);
                    let notice = if removed.is_empty() {
                        "Workspace changed while confirming; nothing was removed. Try again."
                            .to_owned()
                    } else if removed.len() == 1 {
                        "Workspace registration removed.".to_owned()
                    } else {
                        format!("{} workspace registrations removed.", removed.len())
                    };
                    if screen == Screen::Welcome {
                        welcome.set_notice(Some(notice));
                    } else {
                        // Missing-workspace prompts are only created by Welcome
                        // and Open actions, so every non-Welcome prompt belongs
                        // to the Open screen.
                        open.set_notice(Some(notice));
                    }
                }
                Key::Char('n' | 'N') | Key::Escape | Key::Enter => {
                    missing_workspace_prompt = None;
                }
                Key::Quit | Key::CtrlQ => return Ok(Exit::Quit),
                _ => {}
            }
            drawn_material = None;
            continue;
        }
        match screen {
            Screen::Welcome => match step_welcome(&mut welcome, key) {
                WelcomeStep::Stay => {}
                WelcomeStep::Quit => return Ok(Exit::Quit),
                WelcomeStep::OpenList => screen = Screen::Open,
                WelcomeStep::NewForm => {
                    if pending_create
                        .as_ref()
                        .is_some_and(|pending| pending.cancelled)
                    {
                        new_form
                            .set_notice(Some("previous creation is still finishing".to_owned()));
                    }
                    screen = Screen::New;
                }
                WelcomeStep::ConfigScreen => {
                    config_form = Config::load_with_available_models(settings, available_models);
                    screen = Screen::Config;
                }
                WelcomeStep::OpenRecent(index) => {
                    // `Welcome` only creates this action for a visible Recent
                    // number, so the index is fenced by the same model.
                    let recent = &welcome.recent()[index];
                    let paths = recent_paths(recent);
                    if paths.is_empty() {
                        continue;
                    }
                    match loader.missing_paths(&paths) {
                        Ok(missing) if !missing.is_empty() => {
                            missing_workspace_prompt = Some(MissingWorkspacePrompt::new(missing));
                            continue;
                        }
                        Ok(_) => {}
                        Err(error) => {
                            welcome.set_notice(Some(error.to_string()));
                            continue;
                        }
                    }
                    // A workspace this daemon does not serve keeps the switcher on
                    // screen with the reason, so another Recent entry can be tried.
                    let (snapshots, snapshot, deck) =
                        match prepare_workspace_deck(term, loader, &paths) {
                            Ok(prepared) => prepared,
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                                welcome.set_notice(Some(
                                    "Workspace opening was cancelled.".to_owned(),
                                ));
                                continue;
                            }
                            Err(error) => match open_failure_notice(&error) {
                                Some(notice) => {
                                    welcome.set_notice(Some(notice));
                                    continue;
                                }
                                None => return Err(error),
                            },
                        };
                    welcome.set_notice(None);
                    for snapshot in &snapshots {
                        welcome.record_opened(&snapshot.workspace);
                        open.record_opened(&snapshot.workspace);
                    }
                    if let Some(exit) = enter_workspace_deck(
                        term,
                        snapshot,
                        deck,
                        &registry,
                        loader,
                        settings,
                        backend_factory,
                        available_models,
                    )? {
                        return Ok(exit);
                    }
                    screen = Screen::Welcome;
                }
            },
            Screen::Open => match step_open(&mut open, key) {
                OpenStep::Stay => {}
                OpenStep::Quit => return Ok(Exit::Quit),
                OpenStep::Back => screen = Screen::Welcome,
                OpenStep::Choose(paths) => {
                    match loader.missing_paths(&paths) {
                        Ok(missing) if !missing.is_empty() => {
                            missing_workspace_prompt = Some(MissingWorkspacePrompt::new(missing));
                            continue;
                        }
                        Ok(_) => {}
                        Err(error) => {
                            open.set_notice(Some(error.to_string()));
                            continue;
                        }
                    }
                    // Same contract as Recent: the list stays up with the reason so
                    // the workspace this daemon does serve can be chosen instead.
                    let (snapshots, snapshot, deck) =
                        match prepare_workspace_deck(term, loader, &paths) {
                            Ok(prepared) => prepared,
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                                open.set_notice(Some(
                                    "Workspace opening was cancelled.".to_owned(),
                                ));
                                continue;
                            }
                            Err(error) => match open_failure_notice(&error) {
                                Some(notice) => {
                                    open.set_notice(Some(notice));
                                    continue;
                                }
                                None => return Err(error),
                            },
                        };
                    open.set_notice(None);
                    for snapshot in &snapshots {
                        welcome.record_opened(&snapshot.workspace);
                        open.record_opened(&snapshot.workspace);
                    }
                    // Leaving returns to Welcome, not to the list that was used
                    // to get here: all three entries share one way back so the
                    // switcher is always reachable from a workspace.
                    if let Some(exit) = enter_workspace_deck(
                        term,
                        snapshot,
                        deck,
                        &registry,
                        loader,
                        settings,
                        backend_factory,
                        available_models,
                    )? {
                        return Ok(exit);
                    }
                    screen = Screen::Welcome;
                }
                OpenStep::ConfirmCleanup => {
                    let removed = loader.cleanup_missing(&open.workspaces())?;
                    open.remove_paths(&removed);
                    remove_registry_paths(&mut registry, &removed);
                }
                OpenStep::ConfirmUnregister(path) => {
                    let removed = loader.unregister(&[path])?;
                    open.remove_paths(&removed);
                    remove_registry_paths(&mut registry, &removed);
                }
            },
            Screen::New => match step_new(&mut new_form, key) {
                NewStep::Stay => {}
                NewStep::Quit => return Ok(Exit::Quit),
                NewStep::Back => {
                    if let Some(pending) = pending_create.as_mut() {
                        pending.cancelled = true;
                        new_form.finish_create();
                    }
                    screen = Screen::Welcome;
                }
                NewStep::CompleteDirectory(completion) => {
                    let entries = loader
                        .directory_names(completion.parent())
                        .unwrap_or_default();
                    new_form.finish_directory_completion(&completion, entries);
                }
                NewStep::Create(request) => {
                    if pending_create.is_some() {
                        new_form
                            .set_notice(Some("previous creation is still finishing".to_owned()));
                        continue;
                    }
                    let token = WorkspaceCreateToken::new(next_create_token);
                    next_create_token = next_create_token.wrapping_add(1);
                    let effect = WorkspaceCreateEffect {
                        token,
                        request: request.clone(),
                    };
                    match loader.dispatch_create(effect) {
                        Ok(()) => {
                            pending_create = Some(PendingWorkspaceCreate {
                                token,
                                request,
                                cancelled: false,
                            });
                            new_form.begin_create();
                        }
                        Err(error) => {
                            new_form.set_notice(Some(new_project_notice(&error)));
                        }
                    }
                }
            },
            Screen::Config => match step_config(&mut config_form, key, settings) {
                ConfigStep::Stay => {}
                ConfigStep::Quit => return Ok(Exit::Quit),
                ConfigStep::Back => screen = Screen::Welcome,
                ConfigStep::Save => {
                    // The save wave and its `done` hold draw straight to the
                    // terminal, so whatever the gate remembers is no longer on
                    // screen and the next tick must redraw unconditionally.
                    drawn_material = None;
                    if save_config_responsive(term, &mut config_form, settings, None)? {
                        // Hold the `done` confirmation briefly, then return home
                        // with no key press. A failed write skips this and leaves
                        // Config on screen with the error for retry.
                        let (height, width) = term.size()?;
                        term.draw(&super::views::config::render(height, width, &config_form))?;
                        term.wait(super::views::config::DONE_DISPLAY)?;
                        config_form.reset_save();
                        screen = Screen::Welcome;
                    }
                }
                ConfigStep::SaveSource => {
                    drawn_material = None;
                    let _ = save_config_source_responsive(term, &mut config_form, settings);
                }
            },
        }
    }
}

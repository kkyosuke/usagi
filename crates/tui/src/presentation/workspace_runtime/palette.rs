//! Reuse normal navigation and command editors after checking exact targets.

use super::{
    AppEvent, AppKey, Overlay, PaneKind, PaneTab, Target, WorkspaceId, WorkspaceRuntime,
    tab_selection,
};
use crate::presentation::workspace_deck::palette::{Action, CachedTab};
use crate::usecase::{closeup, overview};

impl WorkspaceRuntime {
    pub(crate) fn palette_tabs(&self) -> Vec<CachedTab> {
        std::iter::once(Target::Root(self.state.workspace()))
            .chain(self.state.sessions().iter().copied().map(Target::Session))
            .flat_map(|target| {
                self.panes.pane(target).into_iter().flat_map(move |pane| {
                    pane.tabs()
                        .iter()
                        .enumerate()
                        .filter_map(move |(index, tab)| {
                            let kind = match tab {
                                PaneTab::Pending(pane) | PaneTab::Ready(pane) => pane.kind,
                                PaneTab::Live(pane) => pane.kind,
                                PaneTab::Interrupted(_) => PaneKind::Agent,
                            };
                            let label = match kind {
                                PaneKind::Agent => "Agent",
                                PaneKind::Terminal => "Terminal",
                                PaneKind::Diff => return None,
                            };
                            let state = match tab {
                                PaneTab::Pending(_) => " · starting",
                                PaneTab::Interrupted(_) => " · interrupted",
                                PaneTab::Live(_) | PaneTab::Ready(_) => "",
                            };
                            Some(CachedTab {
                                action: Action::Tab {
                                    target,
                                    selection: tab_selection(tab),
                                    kind,
                                },
                                label: format!("Tab · {label} {}{state}", index + 1),
                            })
                        })
                })
            })
            .collect()
    }

    /// A frontmost search may be cancelled without losing the underlying draft.
    pub(crate) fn palette_navigation_blocked(&self) -> bool {
        self.state.overlay().is_some_and(|overlay| {
            !matches!(
                overlay,
                Overlay::Overview | Overlay::Closeup | Overlay::Garden
            )
        }) || self
            .overview_modal
            .as_ref()
            .is_some_and(|modal| !modal.input().is_empty())
            || self
                .closeup_modal
                .as_ref()
                .is_some_and(|modal| !modal.input().is_empty())
            || self.state.director_new() != super::DirectorNew::Idle
    }

    pub(crate) fn palette_action_available(&self, workspace: WorkspaceId, action: &Action) -> bool {
        if workspace != self.state.workspace() {
            return false;
        }
        if let Some(session) = action.session()
            && (!self.state.sessions().contains(&session)
                || self
                    .state
                    .session_lifecycles()
                    .get(&session)
                    .is_some_and(|projection| !projection.lifecycle.capabilities().can_use))
        {
            return false;
        }
        match action {
            Action::Workspace | Action::Session(_) => true,
            Action::Artifact { target, path } => {
                (matches!(target, Target::Session(_)) || *target == Target::Root(workspace))
                    && !path.is_empty()
                    && std::path::Path::new(path)
                        .components()
                        .all(|part| matches!(part, std::path::Component::Normal(_)))
            }
            Action::Command {
                session: Some(_),
                name,
            } => closeup::commands().any(|command| command.name == *name && command.name != "diff"),
            Action::Command {
                session: None,
                name,
            } => overview::commands().any(|command| command.name == *name),
            Action::Tab { .. } => self.palette_tabs().iter().any(|tab| tab.action == *action),
        }
    }

    pub(crate) fn activate_palette_action(
        &mut self,
        workspace: WorkspaceId,
        action: &Action,
    ) -> Result<Vec<crate::usecase::application::controller::Effect>, &'static str> {
        self.validate_palette_action(workspace, action)?;
        Ok(self.focus_palette_action(action))
    }

    pub(crate) fn validate_palette_action(
        &self,
        workspace: WorkspaceId,
        action: &Action,
    ) -> Result<(), &'static str> {
        if self.palette_navigation_blocked() {
            return Err("Close the palette and finish the current dialog or draft first.");
        }
        if !self.palette_action_available(workspace, action) {
            return Err("Target is no longer available; open the palette and choose again.");
        }
        Ok(())
    }

    fn focus_palette_action(
        &mut self,
        action: &Action,
    ) -> Vec<crate::usecase::application::controller::Effect> {
        if self.state.overlay().is_some() {
            let _ = self.apply_event(AppEvent::Key(AppKey::Escape));
        }
        if self.state.root_terminal_drawer_open() {
            let _ = self.apply_event(AppEvent::Key(AppKey::ToggleRootTerminalDrawer));
        }
        if self.state.director_drawer_open() {
            let _ = self.apply_event(AppEvent::Key(AppKey::ToggleDirectorDrawer));
        }
        if let Some(session) = action.session() {
            let _ = self.apply_event(AppEvent::VisitSession(session));
        }
        match action {
            Action::Workspace => {
                let _ = self.apply_event(AppEvent::Key(AppKey::CtrlO));
            }
            Action::Session(_) => {}
            Action::Artifact { target, path } => {
                return self.apply_event(AppEvent::OpenArtifact {
                    target: *target,
                    path: path.clone(),
                });
            }
            Action::Tab {
                target,
                selection,
                kind,
            } => {
                if matches!(target, Target::Root(_)) {
                    let key = if *kind == PaneKind::Agent {
                        AppKey::ToggleDirectorDrawer
                    } else {
                        AppKey::ToggleRootTerminalDrawer
                    };
                    // Only focus an already validated tab. The drawer's generic
                    // open effect must not launch a second shell for a pending tab.
                    let _ = self.apply_event(AppEvent::Key(key));
                    if *kind == PaneKind::Agent {
                        let _ = self.apply_event(AppEvent::Key(AppKey::OpenDirectorConsole));
                    }
                }
                let _ = self.select_tab_selection(selection.clone());
            }
            Action::Command {
                session: Some(_),
                name,
            } => {
                let _ = self.apply_event(AppEvent::Key(AppKey::OpenCloseupOverlay));
                if let Some(modal) = self.closeup_modal.as_mut() {
                    modal.paste(name);
                }
            }
            Action::Command {
                session: None,
                name,
            } => {
                let _ = self.apply_event(AppEvent::Key(AppKey::OpenOverview));
                if let Some(modal) = self.overview_modal.as_mut() {
                    modal.paste(name);
                }
            }
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests;

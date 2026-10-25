//! One sidebar layout shared by drawing and pointer resolution.

use super::{
    CHROME_ROWS, CREATE_SKELETON_ROWS, DateTime, HomeProjection, LEFT_WIDTH, Selection,
    SessionLifecycle, Target, Utc, abnormal_daemon_speech, home_row_height_at, home_viewport_start,
    panes, pr_summary, sidecar_labels_with_icon_mode, widgets,
};
use crate::usecase::application::controller::SidebarPointerTarget;

pub(super) struct SidebarRow {
    pub selection: Option<Selection>,
    pub top: usize,
    pub height: usize,
}

pub(super) struct SidebarLayout {
    pub rows: Vec<SidebarRow>,
    pub content_capacity: usize,
    pub mascot: Option<widgets::mascot::MascotBlock>,
}

impl SidebarLayout {
    #[must_use]
    pub fn new(height: usize, width: usize, home: &HomeProjection, now: DateTime<Utc>) -> Self {
        let selections = home.rows();
        if height <= 1 {
            return Self {
                rows: if height == 1 {
                    vec![SidebarRow {
                        selection: Some(selections[0]),
                        top: 0,
                        height: 1,
                    }]
                } else {
                    Vec::new()
                },
                content_capacity: height,
                mascot: None,
            };
        }
        let body_capacity = height - 1;
        let sidecar = sidecar_labels_with_icon_mode(
            width,
            home.metrics.as_ref(),
            home.health.evaluate(now.timestamp_millis()),
            home.session_states,
            home.icon_mode,
        );
        let daemon_speech = abnormal_daemon_speech(home.feedback.as_ref());
        let speech = home.mascot_speech.as_ref().or(daemon_speech.as_ref());
        let mascot =
            widgets::mascot::sidebar_block_with_sidecar(width, home.mascot_tick, speech, &sidecar)
                .filter(|block| body_capacity >= block.reserved_rows() + 2);
        let content_capacity = body_capacity.saturating_sub(
            mascot
                .as_ref()
                .map_or(0, widgets::mascot::MascotBlock::reserved_rows),
        );
        let skeleton = usize::from(home.create_pending.is_some()) * CREATE_SKELETON_ROWS;
        let viewport_capacity = content_capacity.saturating_sub(skeleton);
        let selected = selections
            .iter()
            .position(|row| *row == home.selected)
            .unwrap_or(0);
        let start = home_viewport_start(width, home, &selections, selected, viewport_capacity);
        let mut rows = Vec::new();
        let (mut top, mut used) = (0, 0);
        for selection in &selections[start..] {
            if *selection == Selection::NewSession && skeleton > 0 {
                rows.push(SidebarRow {
                    selection: None,
                    top,
                    height: skeleton.min(content_capacity.saturating_sub(top)),
                });
                top += skeleton;
            }
            let height = home_row_height_at(width, home, *selection);
            if used + height > viewport_capacity {
                break;
            }
            rows.push(SidebarRow {
                selection: Some(*selection),
                top,
                height,
            });
            top += height;
            used += height;
        }
        Self {
            rows,
            content_capacity,
            mascot,
        }
    }
}

/// Resolve a cell against the exact projection used to draw the Home frame.
#[must_use]
pub fn sidebar_pointer_target(
    raw_height: usize,
    raw_width: usize,
    home: &HomeProjection,
    now: DateTime<Utc>,
    column: u16,
    row: u16,
) -> Option<SidebarPointerTarget> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let left = panes::split(width, LEFT_WIDTH).left;
    let column = usize::from(column);
    let row = usize::from(row).checked_sub(CHROME_ROWS)?;
    if column >= left {
        return None;
    }
    let layout = SidebarLayout::new(height.saturating_sub(CHROME_ROWS), left, home, now);
    let hit = layout
        .rows
        .iter()
        .find(|hit| (hit.top..hit.top + hit.height).contains(&row))?;
    let selection = hit.selection?;
    if let Selection::Target(Target::Session(id)) = selection {
        let session = home.sessions.iter().find(|session| session.id == id)?;
        // These rows replace the metadata and Agent actions with status or
        // blank lines, even before the daemon publishes a new lifecycle.
        if session.removing || session.lifecycle == SessionLifecycle::Failed {
            return Some(SidebarPointerTarget::Row(selection));
        }
        let line = row - hit.top;
        if line == 1
            && let Some(badge) = pr_summary(session.pr_count, home.icon_mode)
            && column >= left.saturating_sub(widgets::display_width(&badge))
        {
            return Some(SidebarPointerTarget::PullRequests(id));
        }
        if line == 2
            && let Some(label) = home.artifacts_action(session, left)
            && column >= left - widgets::display_width(&label)
        {
            return Some(SidebarPointerTarget::Artifacts(id));
        }
    }
    Some(SidebarPointerTarget::Row(selection))
}

#[cfg(test)]
mod tests {
    use super::super::{
        Feedback, PathBuf, ProjectedSession, SessionId, SessionRecord, WorkspaceId, render_home_at,
    };
    use super::*;
    use crate::usecase::application::controller::{
        AppEvent, AppKey, AppState, BackendEvent, SafeError, SafeMessage, update,
    };
    use usagi_core::domain::session::SessionOrigin;
    use usagi_core::domain::settings::IconMode;

    fn left(line: &str, width: usize) -> String {
        let plain = widgets::strip_ansi(line);
        let mut result = String::new();
        for character in plain.chars() {
            if widgets::display_width(&result) + widgets::display_width(&character.to_string())
                > width
            {
                break;
            }
            result.push(character);
        }
        result
    }

    fn action_home(
        lifecycle: SessionLifecycle,
        removing: bool,
        icon_mode: IconMode,
    ) -> (SessionId, HomeProjection) {
        let workspace = WorkspaceId::new();
        let id = SessionId::new();
        let state = AppState::home(workspace, vec![id]);
        let mut session = ProjectedSession::from_record(
            id,
            &SessionRecord {
                name: "working".into(),
                display_name: None,
                origin: SessionOrigin::Human,
                started_from: None,
                root: PathBuf::from("."),
                created_at: DateTime::default(),
                last_active: None,
                notes: usagi_core::domain::note::Scratchpad::default(),
                prs: Vec::new(),
            },
        );
        session.lifecycle = lifecycle;
        session.removing = removing;
        session.pr_count = 1;
        (
            id,
            HomeProjection::from_state(&state, "test", &[session]).with_icon_mode(icon_mode),
        )
    }

    #[test]
    fn sidebar_action_cells_are_inert_when_the_action_rows_are_hidden() {
        let now = DateTime::from_timestamp(0, 0).unwrap();
        for (lifecycle, removing) in [
            (SessionLifecycle::Available, true),
            (SessionLifecycle::Failed, false),
            (SessionLifecycle::Failed, true),
        ] {
            for mode in [IconMode::NerdFont, IconMode::Text] {
                let (id, home) = action_home(lifecycle, removing, mode);
                let frame = render_home_at(30, 100, &home, now);
                for row in [3, 4] {
                    if removing {
                        assert!(left(&frame[row], LEFT_WIDTH).trim().is_empty());
                    }
                    assert_eq!(
                        sidebar_pointer_target(
                            30,
                            100,
                            &home,
                            now,
                            35,
                            u16::try_from(row).unwrap()
                        ),
                        Some(SidebarPointerTarget::Row(Selection::Target(
                            Target::Session(id)
                        )))
                    );
                }
            }
        }
    }

    #[test]
    fn sidebar_action_pr_badges_stay_clickable_for_each_rendered_lifecycle() {
        let now = DateTime::from_timestamp(0, 0).unwrap();
        for lifecycle in [
            SessionLifecycle::Creating,
            SessionLifecycle::Initializing,
            SessionLifecycle::Available,
            SessionLifecycle::Deleting,
        ] {
            for mode in [IconMode::NerdFont, IconMode::Text] {
                let (id, home) = action_home(lifecycle, false, mode);
                let frame = render_home_at(30, 100, &home, now);
                assert!(left(&frame[3], LEFT_WIDTH).ends_with('1'));
                assert_eq!(
                    sidebar_pointer_target(30, 100, &home, now, 35, 3),
                    Some(SidebarPointerTarget::PullRequests(id)),
                    "{lifecycle:?}, {mode:?}"
                );
            }
        }
    }

    #[test]
    fn artifact_hitbox_matches_the_visible_badge_and_disappears_with_empty_counts() {
        let now = DateTime::from_timestamp(0, 0).unwrap();
        for mode in [IconMode::NerdFont, IconMode::Text] {
            let (id, home) = action_home(SessionLifecycle::Available, false, mode);
            for count in [0, 1, 12] {
                let home = home.clone().with_artifact_counts(std::sync::Arc::new(
                    std::collections::BTreeMap::from([(id, count)]),
                ));
                let label = super::super::artifacts_summary(count, mode);
                let start = label
                    .as_ref()
                    .map(|label| LEFT_WIDTH - widgets::display_width(label));
                for column in 0..LEFT_WIDTH {
                    let target = sidebar_pointer_target(
                        30,
                        100,
                        &home,
                        now,
                        u16::try_from(column).unwrap(),
                        4,
                    );
                    let expected = if start.is_some_and(|start| column >= start) {
                        SidebarPointerTarget::Artifacts(id)
                    } else {
                        SidebarPointerTarget::Row(Selection::Target(Target::Session(id)))
                    };
                    assert_eq!(
                        target,
                        Some(expected),
                        "{mode:?}, count {count}, column {column}"
                    );
                }
            }
        }
    }

    #[test]
    fn rendered_artifact_cells_resolve_to_the_same_session_in_every_viewport() {
        let workspace = WorkspaceId::new();
        let ids = (0..6).map(|_| SessionId::new()).collect::<Vec<_>>();
        let sessions = ids
            .iter()
            .enumerate()
            .map(|(index, id)| {
                ProjectedSession::from_record(
                    *id,
                    &SessionRecord {
                        name: format!("session-{index}"),
                        display_name: None,
                        origin: SessionOrigin::Human,
                        started_from: None,
                        root: PathBuf::from("."),
                        created_at: chrono::DateTime::default(),
                        last_active: None,
                        notes: usagi_core::domain::note::Scratchpad::default(),
                        prs: Vec::new(),
                    },
                )
            })
            .collect::<Vec<_>>();
        let now = DateTime::from_timestamp(0, 0).unwrap();
        for mode in [IconMode::NerdFont, IconMode::Text] {
            for scenario in 0..6 {
                for width in [18, 25, 38, 100] {
                    for height in [3, 4, 10, 14, 18, 24, 30] {
                        for selected in 0..=ids.len() {
                            let mut state = AppState::home(workspace, ids.clone());
                            for _ in 0..selected {
                                let _ = update(&mut state, AppEvent::Key(AppKey::Down));
                            }
                            let error = || SafeError {
                                message: SafeMessage::new(
                                    "A long diagnostic message that wraps across narrow sidebar columns",
                                ),
                                error_id: "review".into(),
                            };
                            let feedback = match scenario {
                                1 => Some(Feedback::Disconnected),
                                2 => Some(Feedback::ResyncRequired),
                                3 => Some(Feedback::OperationError(error())),
                                4 => Some(Feedback::TerminalError(error())),
                                _ => None,
                            };
                            if let Some(feedback) = feedback {
                                let _ = update(
                                    &mut state,
                                    AppEvent::Backend(BackendEvent::Feedback(feedback)),
                                );
                            }
                            let mut home = HomeProjection::from_state(&state, "test", &sessions)
                                .with_icon_mode(mode)
                                .with_artifact_counts(std::sync::Arc::new(
                                    ids.iter().map(|id| (*id, 1)).collect(),
                                ));
                            if scenario == 5 {
                                home = home.with_create_pending(Some("pending".into()));
                            }
                            let frame = render_home_at(height, width, &home, now);
                            let sidebar = panes::split(width, LEFT_WIDTH).left;
                            for row in 0..height {
                                let label = super::super::artifacts_summary(1, mode).unwrap();
                                let visible = left(&frame[row], sidebar).ends_with(&label);
                                let target = sidebar_pointer_target(
                                    height,
                                    width,
                                    &home,
                                    now,
                                    u16::try_from(sidebar - 1).unwrap(),
                                    u16::try_from(row).unwrap(),
                                );
                                if visible {
                                    let summary = left(&frame[row - 2], sidebar);
                                    let id = ids
                                        .iter()
                                        .enumerate()
                                        .find_map(|(index, id)| {
                                            summary
                                                .contains(&format!("session-{index}"))
                                                .then_some(*id)
                                        })
                                        .unwrap();
                                    assert_eq!(
                                        target,
                                        Some(SidebarPointerTarget::Artifacts(id)),
                                        "{mode:?}, scenario {scenario}, {width}x{height}, cursor {selected}, row {row}"
                                    );
                                } else {
                                    assert!(
                                        !matches!(target, Some(SidebarPointerTarget::Artifacts(_))),
                                        "invisible action: scenario {scenario}, {width}x{height}, cursor {selected}, row {row}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

//! Process-level attention list. Snapshot failures never become zero work.
mod view;

pub(super) use view::{handle_input, render};

use super::{
    AttentionDestination, DeckOverlay, HashSet, Key, OverlayIntent, Path, PathBuf, Role, SessionId,
    WorkspaceDeck, WorkspaceId, WorkspaceSlot, fuzzy_score, modal, widgets,
};
use crate::presentation::theme::Style;
use usagi_core::domain::{
    attention::{AttentionKind, WorkspaceAttention},
    presentation_text::sanitize_presentation_line,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum InboxView {
    #[default]
    Actions,
    Updates,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct AttentionList {
    selected: Option<(WorkspaceId, String)>,
    filter: String,
    view: InboxView,
}

struct Row {
    identity: (WorkspaceId, String),
    context: String,
    reason: String,
    action: &'static str,
    open_label: &'static str,
    intent: OverlayIntent,
    rank: u8,
}

fn rows(slots: &[WorkspaceSlot], filter: &str, view: InboxView) -> Vec<Row> {
    let mut rows = Vec::new();
    for slot in slots {
        let project = sanitize_presentation_line(&slot.label);
        if let Some(snapshot) = slot.attention.as_ref().filter(|_| !slot.attention_stale) {
            for item in &snapshot.items {
                if item.kind.needs_action() != (view == InboxView::Actions) {
                    continue;
                }
                let destination = if item.kind == AttentionKind::Decision {
                    AttentionDestination::Decisions
                } else if item.key.starts_with("pr:") || item.kind == AttentionKind::Review {
                    AttentionDestination::PullRequests
                } else {
                    AttentionDestination::Session
                };
                let intent = OverlayIntent::AttentionVisit {
                    path: slot.path.clone(),
                    workspace: slot.workspace_id,
                    session: item.session,
                    destination,
                };
                rows.push(Row {
                    identity: (slot.workspace_id, item.key.clone()),
                    context: format!("{} / {}", project, sanitize_presentation_line(&item.label)),
                    reason: sanitize_presentation_line(&item.reason),
                    action: match item.kind {
                        AttentionKind::Decision => "Answer question",
                        AttentionKind::Review => "Review PR",
                        AttentionKind::Blocked
                            if destination == AttentionDestination::PullRequests =>
                        {
                            "Check PR problem"
                        }
                        AttentionKind::Blocked => "Check stopped task",
                        AttentionKind::System => "Waiting",
                        AttentionKind::Running => "Running",
                    },
                    open_label: match destination {
                        AttentionDestination::Decisions => "Open questions",
                        AttentionDestination::PullRequests => "Open PR",
                        AttentionDestination::Session => "Open session",
                    },
                    intent,
                    rank: match item.kind {
                        AttentionKind::Decision => 0,
                        AttentionKind::Review => 1,
                        AttentionKind::Blocked => 2,
                        AttentionKind::System => 3,
                        AttentionKind::Running => 4,
                    },
                });
            }
        }
    }
    rows.retain(|row| {
        filter.is_empty()
            || fuzzy_score(
                &format!("{} {} {}", row.action, row.context, row.reason),
                filter,
            )
            .is_some()
    });
    rows.sort_by_key(|row| row.rank);
    rows
}

impl AttentionList {
    fn select_first(&mut self, slots: &[WorkspaceSlot]) {
        self.selected = rows(slots, &self.filter, self.view)
            .first()
            .map(|row| row.identity.clone());
    }

    fn set_view(&mut self, view: InboxView, slots: &[WorkspaceSlot]) {
        self.view = view;
        self.select_first(slots);
    }

    fn index(&self, rows: &[Row]) -> usize {
        self.selected
            .as_ref()
            .and_then(|selected| rows.iter().position(|row| &row.identity == selected))
            .unwrap_or(0)
    }

    pub(super) fn handle(&mut self, key: &Key, slots: &[WorkspaceSlot]) -> OverlayIntent {
        let next_view = match key {
            Key::Left => Some(InboxView::Actions),
            Key::Right => Some(InboxView::Updates),
            Key::Tab => Some(if self.view == InboxView::Actions {
                InboxView::Updates
            } else {
                InboxView::Actions
            }),
            _ => None,
        };
        if let Some(view) = next_view {
            self.set_view(view, slots);
            return OverlayIntent::Stay;
        }
        let rows = rows(slots, &self.filter, self.view);
        let index = self.index(&rows);
        let next = match key {
            Key::Up => index.saturating_sub(1),
            Key::Down => (index + 1).min(rows.len().saturating_sub(1)),
            Key::PageUp => index.saturating_sub(8),
            Key::PageDown => (index + 8).min(rows.len().saturating_sub(1)),
            Key::Enter => {
                return rows
                    .get(index)
                    .filter(|row| {
                        self.selected
                            .as_ref()
                            .is_none_or(|selected| *selected == row.identity)
                    })
                    .map_or(OverlayIntent::Stay, |row| row.intent.clone());
            }
            Key::Escape => return OverlayIntent::Cancel,
            Key::Char(character) => {
                self.filter.push(*character);
                self.select_first(slots);
                return OverlayIntent::Stay;
            }
            Key::Paste(text) => {
                self.filter.push_str(&sanitize_presentation_line(text));
                self.select_first(slots);
                return OverlayIntent::Stay;
            }
            Key::Backspace => {
                self.filter.pop();
                self.select_first(slots);
                return OverlayIntent::Stay;
            }
            _ => return OverlayIntent::Stay,
        };
        self.selected = rows.get(next).map(|row| row.identity.clone());
        OverlayIntent::Stay
    }
}

pub(super) fn badge(slot: &WorkspaceSlot) -> String {
    match &slot.attention {
        _ if slot.attention_stale => " ?".into(),
        None => " ?".into(),
        Some(snapshot) => {
            let count = snapshot
                .items
                .iter()
                .filter(|item| item.kind.needs_action())
                .count();
            if count == 0 {
                String::new()
            } else {
                format!(" !{count}")
            }
        }
    }
}

#[derive(Default)]
struct Summary {
    actions: usize,
    updates: usize,
    unknown: usize,
}

impl Summary {
    fn from_slots(slots: &[WorkspaceSlot]) -> Self {
        let mut summary = Self::default();
        for slot in slots {
            if let Some(snapshot) = slot.attention.as_ref().filter(|_| !slot.attention_stale) {
                for item in &snapshot.items {
                    if item.kind.needs_action() {
                        summary.actions += 1;
                    } else {
                        summary.updates += 1;
                    }
                }
            } else {
                summary.unknown += 1;
            }
        }
        summary
    }

    fn action_total(&self) -> String {
        match (self.actions, self.unknown > 0) {
            (0, true) => "?".into(),
            (count, true) => format!("{count}+"),
            (count, false) => count.to_string(),
        }
    }
}

/// The bar counts only fresh work that needs a person. Unknown projects keep
/// the total visibly incomplete instead of turning missing observations into zero.
pub(super) fn indicator(slots: &[WorkspaceSlot]) -> (String, Style) {
    let summary = Summary::from_slots(slots);
    if summary.actions == 0 {
        return (
            if summary.unknown > 0 {
                "Checking status…"
            } else {
                "No action needed"
            }
            .into(),
            Style::new().dim(),
        );
    }
    (
        format!("! Action needed ({})", summary.action_total()),
        Role::Warning.style().bold(),
    )
}

impl WorkspaceDeck {
    pub fn schedule_attention_visit(
        &mut self,
        path: PathBuf,
        session: Option<SessionId>,
        destination: AttentionDestination,
    ) {
        self.pending_attention = Some((path, session, destination));
    }

    pub fn take_attention_visit(
        &mut self,
        path: &Path,
    ) -> Option<(Option<SessionId>, AttentionDestination)> {
        if self
            .pending_attention
            .as_ref()
            .is_some_and(|(target, _, _)| target == path)
        {
            self.pending_attention
                .take()
                .map(|(_, session, destination)| (session, destination))
        } else {
            None
        }
    }

    pub fn open_attention(&mut self) {
        let selected = rows(&self.slots, "", InboxView::Actions)
            .first()
            .map(|row| row.identity.clone());
        self.overlay = Some(DeckOverlay::Attention(AttentionList {
            selected,
            filter: String::new(),
            view: InboxView::Actions,
        }));
        self.notice = None;
    }

    /// Fair, bounded rounds include the active tab; no project is starved after 16.
    pub fn attention_workspaces(&mut self) -> Vec<WorkspaceId> {
        let count = self.slots.len();
        if count == 0 {
            return Vec::new();
        }
        let targets = (0..count.min(16))
            .map(|offset| self.slots[(self.attention_cursor + offset) % count].workspace_id)
            .collect();
        self.attention_cursor = (self.attention_cursor + count.min(16)) % count;
        targets
    }

    /// A stalled observation worker cannot leave an indefinitely fresh count.
    pub fn expire_attention(&mut self, now: std::time::Instant) -> bool {
        let mut changed = false;
        for slot in &mut self.slots {
            if !slot.attention_stale
                && slot.attention_observed_at.is_some_and(|seen| {
                    now.saturating_duration_since(seen) >= std::time::Duration::from_secs(30)
                })
            {
                slot.attention_stale = true;
                changed = true;
            }
        }
        changed
    }

    /// Install only complete, identity-checked observations. Keep old rows as stale.
    pub fn apply_attention(
        &mut self,
        workspace: WorkspaceId,
        result: Result<WorkspaceAttention, String>,
    ) -> bool {
        let Some(slot) = self
            .slots
            .iter_mut()
            .find(|slot| slot.workspace_id == workspace)
        else {
            return false;
        };
        let previous = (slot.attention.clone(), slot.attention_stale);
        match result {
            Ok(snapshot) if valid_snapshot(workspace, &snapshot) => {
                slot.attention = Some(snapshot);
                slot.attention_stale = false;
                slot.attention_observed_at = Some(std::time::Instant::now());
            }
            _ => slot.attention_stale = true,
        }
        let changed = previous != (slot.attention.clone(), slot.attention_stale);
        if let Some(DeckOverlay::Attention(list)) = self.overlay.as_mut()
            && list.selected.is_none()
        {
            list.select_first(&self.slots);
        }
        changed
    }
}

fn valid_snapshot(workspace: WorkspaceId, snapshot: &WorkspaceAttention) -> bool {
    let mut keys = HashSet::new();
    snapshot.workspace == workspace
        && snapshot.items.len() <= usagi_core::domain::attention::ATTENTION_ITEMS_MAX
        && snapshot.items.iter().all(|item| {
            !item.key.is_empty()
                && item.key.len() <= 1024
                && item.label.len() <= 1024
                && item.reason.len() <= 1024
                && keys.insert(&item.key)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::workspace_deck::{
        ProjectBarTarget, Workspace, WorkspaceSnapshot, project_bar, render_overlay,
    };
    use chrono::Utc;
    use usagi_core::domain::attention::{AttentionItem, AttentionKind};

    pub(super) fn deck() -> WorkspaceDeck {
        let snapshots = (0..2)
            .map(|n| {
                WorkspaceSnapshot::with_runtime_ids(
                    Workspace {
                        name: format!("project-{n}"),
                        path: PathBuf::from(format!("/project-{n}")),
                        created_at: Utc::now(),
                        updated_at: Utc::now(),
                    },
                    usagi_core::domain::workspace_state::WorkspaceState::default(),
                    WorkspaceId::new(),
                    Vec::new(),
                )
            })
            .collect::<Vec<_>>();
        WorkspaceDeck::from_snapshots(&snapshots).unwrap()
    }
    pub(super) fn item(key: &str, kind: AttentionKind) -> AttentionItem {
        AttentionItem {
            key: key.into(),
            session: Some(SessionId::new()),
            label: "build".into(),
            kind,
            reason: "needs a choice".into(),
        }
    }
    pub(super) fn update(deck: &mut WorkspaceDeck, index: usize, items: Vec<AttentionItem>) {
        let workspace = deck.slots[index].workspace_id;
        deck.apply_attention(workspace, Ok(WorkspaceAttention { workspace, items }));
    }

    #[test]
    fn attention_is_live_across_projects_and_never_turns_an_error_into_zero() {
        let mut deck = deck();
        assert_eq!(badge(&deck.slots[0]), " ?");
        let decision = item("question", AttentionKind::Decision);
        let expected = decision.session.unwrap();
        update(
            &mut deck,
            1,
            vec![
                decision.clone(),
                item("ci", AttentionKind::System),
                item("running", AttentionKind::Running),
            ],
        );
        assert_eq!(badge(&deck.slots[1]), " !1");
        update(&mut deck, 0, Vec::new());
        assert_eq!(badge(&deck.slots[0]), "");
        deck.open_attention();
        assert_eq!(
            deck.handle_overlay_key(&Key::Enter),
            OverlayIntent::AttentionVisit {
                path: deck.slots[1].path.clone(),
                workspace: deck.slots[1].workspace_id,
                session: Some(expected),
                destination: AttentionDestination::Decisions
            }
        );
        let before = deck.clone();
        let workspace = deck.slots[1].workspace_id;
        assert!(!deck.apply_attention(workspace, Ok(deck.slots[1].attention.clone().unwrap())));
        assert_eq!(deck.slots[1].attention, before.slots[1].attention);
        assert!(deck.apply_attention(workspace, Err("offline".into())));
        assert_eq!(badge(&deck.slots[1]), " ?");
        assert_eq!(deck.handle_overlay_key(&Key::Enter), OverlayIntent::Stay);
        let frame = render_overlay(&deck, 24, 120, &vec![String::new(); 24]).join("\n");
        assert!(!frame.contains("needs a choice"));
        assert!(frame.contains("Status unknown for 1 project"));
        assert!(frame.contains("No confirmed actions"));
        update(&mut deck, 1, vec![decision]);
        assert_eq!(badge(&deck.slots[1]), " !1");
        assert!(!deck.apply_attention(WorkspaceId::new(), Err("gone".into())));
    }

    #[test]
    fn attention_rejects_wrong_workspace_duplicates_and_oversized_items() {
        let mut deck = deck();
        let workspace = deck.slots[0].workspace_id;
        let entry = item("entry", AttentionKind::Blocked);
        assert!(deck.apply_attention(
            workspace,
            Ok(WorkspaceAttention {
                workspace: WorkspaceId::new(),
                items: vec![entry.clone()]
            })
        ));
        assert!(deck.slots[0].attention.is_none());
        assert!(!valid_snapshot(
            workspace,
            &WorkspaceAttention {
                workspace,
                items: vec![entry.clone(), entry.clone()]
            }
        ));
        let mut too_long = entry;
        too_long.reason = "x".repeat(1025);
        assert!(!valid_snapshot(
            workspace,
            &WorkspaceAttention {
                workspace,
                items: vec![too_long]
            }
        ));
        assert!(!valid_snapshot(
            workspace,
            &WorkspaceAttention {
                workspace,
                items: vec![item("", AttentionKind::Review)]
            }
        ));
        assert_eq!(deck.handle_overlay_key(&Key::Escape), OverlayIntent::Stay);
    }

    #[test]
    fn attention_preserves_selected_identity_when_rows_move_or_disappear() {
        let mut deck = deck();
        let first = item("a", AttentionKind::Review);
        let second = item("b", AttentionKind::Review);
        update(&mut deck, 0, vec![first.clone(), second.clone()]);
        update(&mut deck, 1, vec![]);
        deck.open_attention();
        deck.handle_overlay_key(&Key::Down);
        update(&mut deck, 0, vec![second.clone(), first.clone()]);
        assert!(
            matches!(deck.handle_overlay_key(&Key::Enter), OverlayIntent::AttentionVisit { session, .. } if session == second.session)
        );
        update(&mut deck, 0, vec![first]);
        assert_eq!(deck.handle_overlay_key(&Key::Enter), OverlayIntent::Stay);
        deck.handle_overlay_key(&Key::Up);
        assert!(matches!(
            deck.handle_overlay_key(&Key::Enter),
            OverlayIntent::AttentionVisit { .. }
        ));
        for key in [
            Key::PageDown,
            Key::PageUp,
            Key::Char('z'),
            Key::Backspace,
            Key::Paste("nomatch".into()),
            Key::Other,
        ] {
            deck.handle_overlay_key(&key);
        }
        assert_eq!(deck.handle_overlay_key(&Key::Enter), OverlayIntent::Stay);
        deck.set_notice("safe error");
        let frame = render_overlay(&deck, 24, 80, &vec![String::new(); 24]).join("\n");
        assert!(frame.contains("No items match this filter"));
        assert!(frame.contains("safe error"));
        assert_eq!(deck.handle_overlay_key(&Key::Escape), OverlayIntent::Cancel);
    }

    #[test]
    fn attention_root_rows_and_bar_button_have_stable_targets() {
        let mut deck = deck();
        let mut root = item("root", AttentionKind::Decision);
        root.session = None;
        update(&mut deck, 1, vec![root]);
        update(
            &mut deck,
            0,
            vec![
                item("system", AttentionKind::System),
                item("running", AttentionKind::Running),
            ],
        );
        deck.open_attention();
        assert_eq!(
            deck.handle_overlay_key(&Key::Enter),
            OverlayIntent::AttentionVisit {
                path: deck.slots[1].path.clone(),
                workspace: deck.slots[1].workspace_id,
                session: None,
                destination: AttentionDestination::Decisions
            }
        );
        let bar = project_bar(&deck, 100);
        assert!(bar.line.contains("!1"));
        assert!(
            bar.hits
                .iter()
                .any(|hit| hit.target == ProjectBarTarget::Attention
                    && bar.target_at(hit.columns.start) == Some(&ProjectBarTarget::Attention))
        );
        let frame = render_overlay(&deck, 24, 120, &vec![String::new(); 24]).join("\n");
        assert!(frame.contains("Updates (2)"));
        assert!(!frame.contains("Waiting"));
        deck.handle_overlay_key(&Key::Right);
        let updates = render_overlay(&deck, 24, 120, &vec![String::new(); 24]).join("\n");
        assert!(updates.contains("Waiting"));
        assert!(updates.contains("Running"));
        let small = render_overlay(&deck, 5, 12, &vec![String::new(); 5]);
        assert_eq!(small.len(), 5);
    }

    #[test]
    fn bar_notice_counts_fresh_actions_and_keeps_unknown_totals_explicit() {
        let mut deck = deck();
        let assert_notice = |deck: &WorkspaceDeck, label: &str, style: Style| {
            let bar = project_bar(deck, 120);
            assert!(bar.line.ends_with(&style.paint(&format!(" {label} "))));
        };
        let warning = Role::Warning.style().bold();
        assert_notice(&deck, "Checking status…", Style::new().dim());
        let work = vec![
            item("question", AttentionKind::Decision),
            item("review", AttentionKind::Review),
            item("blocked", AttentionKind::Blocked),
            item("ci", AttentionKind::System),
            item("agent", AttentionKind::Running),
        ];
        update(&mut deck, 0, work);
        assert_notice(&deck, "! Action needed (3+)", warning);
        update(&mut deck, 1, vec![item("other", AttentionKind::Decision)]);
        assert_notice(&deck, "! Action needed (4)", warning);

        let workspace = deck.slots[0].workspace_id;
        deck.apply_attention(workspace, Err("offline".into()));
        assert_notice(&deck, "! Action needed (1+)", warning);
        update(&mut deck, 1, vec![]);
        assert_notice(&deck, "Checking status…", Style::new().dim());
        update(
            &mut deck,
            0,
            vec![
                item("ci", AttentionKind::System),
                item("agent", AttentionKind::Running),
            ],
        );
        assert_notice(&deck, "No action needed", Style::new().dim());
    }

    #[test]
    fn right_aligned_notice_separates_actions_and_preserves_click_targets() {
        let mut deck = deck();
        "日本語プロジェクト".clone_into(&mut deck.slots[0].label);
        update(
            &mut deck,
            0,
            vec![item("question", AttentionKind::Decision)],
        );
        update(&mut deck, 1, vec![]);
        for width in 0..160 {
            let bar = project_bar(&deck, width);
            assert!(widgets::display_width(&bar.line) <= width);
            for hit in &bar.hits {
                assert!(hit.columns.end <= width);
                for column in hit.columns.clone() {
                    assert_eq!(bar.target_at(column), Some(&hit.target));
                }
            }
            let Some(notice) = bar
                .hits
                .iter()
                .find(|hit| hit.target == ProjectBarTarget::Attention)
            else {
                continue;
            };
            let add = bar
                .hits
                .iter()
                .find(|hit| hit.target == ProjectBarTarget::Add)
                .unwrap();
            assert_eq!(notice.columns.end, width);
            assert!(notice.columns.start >= add.columns.end + 2);
            for column in add.columns.end..notice.columns.start {
                assert_eq!(bar.target_at(column), None);
            }
        }
        let bar = project_bar(&deck, 120);
        assert!(
            bar.line
                .contains(&Role::Warning.style().bold().paint(" !1"))
        );
    }

    #[test]
    fn attention_expires_and_pending_navigation_is_consumed_only_at_its_destination() {
        let mut deck = deck();
        assert!(!deck.expire_attention(std::time::Instant::now()));
        update(&mut deck, 0, vec![item("ci", AttentionKind::System)]);
        let later = std::time::Instant::now() + std::time::Duration::from_secs(31);
        assert!(deck.expire_attention(later));
        assert!(!deck.expire_attention(later));
        assert_eq!(badge(&deck.slots[0]), " ?");
        let path = deck.slots[1].path.clone();
        let session = Some(SessionId::new());
        deck.schedule_attention_visit(path.clone(), session, AttentionDestination::Session);
        assert!(deck.take_attention_visit(Path::new("/wrong")).is_none());
        assert_eq!(
            deck.take_attention_visit(&path),
            Some((session, AttentionDestination::Session))
        );
        assert!(deck.take_attention_visit(&path).is_none());
        deck.schedule_attention_visit(path.clone(), session, AttentionDestination::Session);
        deck.close_path(&path);
        assert!(deck.take_attention_visit(&path).is_none());
    }

    #[test]
    fn attention_renders_loading_and_all_action_categories() {
        let mut deck = deck();
        deck.open_attention();
        let frame = render_overlay(&deck, 24, 120, &vec![String::new(); 24]).join("\n");
        assert!(frame.contains("Status unknown"));
        assert_eq!(deck.handle_overlay_key(&Key::Enter), OverlayIntent::Stay);
        update(
            &mut deck,
            0,
            vec![
                item("blocked", AttentionKind::Blocked),
                item("review", AttentionKind::Review),
            ],
        );
        update(&mut deck, 1, vec![]);
        deck.open_attention();
        let frame = render_overlay(&deck, 24, 120, &vec![String::new(); 24]).join("\n");
        assert!(frame.contains("Action needed (2)"));
        assert!(matches!(
            deck.handle_overlay_key(&Key::Enter),
            OverlayIntent::AttentionVisit {
                destination: AttentionDestination::PullRequests,
                ..
            }
        ));
        deck.handle_overlay_key(&Key::Down);
        assert!(matches!(
            deck.handle_overlay_key(&Key::Enter),
            OverlayIntent::AttentionVisit {
                destination: AttentionDestination::Session,
                ..
            }
        ));
        let workspace = deck.slots[0].workspace_id;
        assert!(!valid_snapshot(
            workspace,
            &WorkspaceAttention {
                workspace,
                items: (0..=usagi_core::domain::attention::ATTENTION_ITEMS_MAX)
                    .map(|n| item(&n.to_string(), AttentionKind::Running))
                    .collect()
            }
        ));
    }

    #[test]
    fn inbox_starts_with_actions_and_keeps_progress_in_its_own_view() {
        let mut deck = deck();
        let question = item("question", AttentionKind::Decision);
        let question_session = question.session;
        let mut ci = item("pr:ci", AttentionKind::System);
        ci.reason = "CI is still running".into();
        update(
            &mut deck,
            0,
            vec![
                question,
                item("review", AttentionKind::Review),
                ci,
                item("agent", AttentionKind::Running),
            ],
        );
        update(&mut deck, 1, vec![item("failed", AttentionKind::Blocked)]);
        deck.open_attention();
        let frame = render_overlay(&deck, 24, 120, &vec![String::new(); 24]).join("\n");
        assert!(frame.contains("Answer question"));
        assert!(frame.contains("Review PR"));
        assert!(frame.contains("Check stopped task"));
        assert!(frame.contains("Enter: Open questions"));
        assert!(!frame.contains("CI is still running"));
        assert!(!frame.contains("Running"));
        assert!(
            matches!(deck.handle_overlay_key(&Key::Enter), OverlayIntent::AttentionVisit { session, destination: AttentionDestination::Decisions, .. } if session == question_session)
        );

        deck.handle_overlay_key(&Key::Right);
        let frame = render_overlay(&deck, 24, 120, &vec![String::new(); 24]).join("\n");
        assert!(frame.contains("CI is still running"));
        assert!(frame.contains("No action needed here"));
        assert!(!frame.contains("Answer question"));
        assert!(matches!(
            deck.handle_overlay_key(&Key::Enter),
            OverlayIntent::AttentionVisit {
                destination: AttentionDestination::PullRequests,
                ..
            }
        ));
        deck.handle_overlay_key(&Key::Tab);
        assert!(matches!(
            deck.handle_overlay_key(&Key::Enter),
            OverlayIntent::AttentionVisit {
                destination: AttentionDestination::Decisions,
                ..
            }
        ));

        deck.handle_overlay_key(&Key::Paste("CI is still running".into()));
        assert_eq!(deck.handle_overlay_key(&Key::Enter), OverlayIntent::Stay);
        deck.handle_overlay_key(&Key::Right);
        assert!(matches!(
            deck.handle_overlay_key(&Key::Enter),
            OverlayIntent::AttentionVisit {
                destination: AttentionDestination::PullRequests,
                ..
            }
        ));
        deck.close_overlay();
        deck.open_attention();
        assert!(matches!(
            deck.handle_overlay_key(&Key::Enter),
            OverlayIntent::AttentionVisit {
                destination: AttentionDestination::Decisions,
                ..
            }
        ));
    }

    #[test]
    fn switching_views_pins_selection_across_reordered_or_removed_items() {
        for key in [Key::Right, Key::Tab] {
            let mut deck = deck();
            let first = item("first", AttentionKind::System);
            let second = item("second", AttentionKind::System);
            update(&mut deck, 0, vec![first.clone(), second.clone()]);
            update(&mut deck, 1, vec![]);
            deck.open_attention();
            deck.handle_overlay_key_at(&key, 24, 120);
            update(&mut deck, 0, vec![second.clone(), first.clone()]);
            assert!(
                matches!(deck.handle_overlay_key_at(&Key::Enter, 24, 120), OverlayIntent::AttentionVisit { session, .. } if session == first.session)
            );
            update(&mut deck, 0, vec![second]);
            deck.handle_overlay_key_at(&Key::Other, 24, 120);
            assert_eq!(
                deck.handle_overlay_key_at(&Key::Enter, 24, 120),
                OverlayIntent::Stay
            );
        }
    }

    #[test]
    fn editing_a_filter_pins_selection_across_reordered_or_removed_items() {
        for key in [Key::Char('b'), Key::Paste("build".into()), Key::Backspace] {
            let mut deck = deck();
            let first = item("first", AttentionKind::Decision);
            let second = item("second", AttentionKind::Decision);
            update(&mut deck, 0, vec![first.clone(), second.clone()]);
            update(&mut deck, 1, vec![]);
            deck.open_attention();
            if key == Key::Backspace {
                deck.handle_overlay_key_at(&Key::Paste("buildx".into()), 24, 120);
            }
            deck.handle_overlay_key_at(&key, 24, 120);
            update(&mut deck, 0, vec![second.clone(), first.clone()]);
            assert!(
                matches!(deck.handle_overlay_key_at(&Key::Enter, 24, 120), OverlayIntent::AttentionVisit { session, .. } if session == first.session)
            );
            update(&mut deck, 0, vec![second]);
            assert_eq!(
                deck.handle_overlay_key_at(&Key::Enter, 24, 120),
                OverlayIntent::Stay
            );
        }
    }

    #[test]
    fn first_observed_items_pin_selection_in_an_empty_or_filtered_inbox() {
        for filter in ["", "build"] {
            let mut deck = deck();
            deck.open_attention();
            deck.handle_overlay_key_at(&Key::Paste(filter.into()), 24, 120);
            let first = item("first", AttentionKind::Decision);
            let second = item("second", AttentionKind::Decision);
            update(&mut deck, 0, vec![first.clone(), second.clone()]);
            update(&mut deck, 0, vec![second.clone(), first.clone()]);
            assert!(
                matches!(deck.handle_overlay_key_at(&Key::Enter, 24, 120), OverlayIntent::AttentionVisit { session, .. } if session == first.session)
            );
            update(&mut deck, 0, vec![second]);
            deck.handle_overlay_key_at(&Key::Other, 24, 120);
            assert_eq!(
                deck.handle_overlay_key_at(&Key::Enter, 24, 120),
                OverlayIntent::Stay
            );
        }
    }

    #[test]
    fn progress_and_unknown_observations_never_claim_a_person_must_act() {
        let mut deck = deck();
        assert_eq!(
            deck.handle_overlay_key_at(&Key::Enter, 24, 80),
            OverlayIntent::Stay
        );
        update(&mut deck, 0, vec![item("agent", AttentionKind::Running)]);
        update(&mut deck, 1, vec![item("ci", AttentionKind::System)]);
        deck.open_attention();
        assert_eq!(
            deck.handle_overlay_key_at(&Key::Enter, 24, 80),
            OverlayIntent::Stay
        );
        assert!(
            render_overlay(&deck, 24, 80, &vec![String::new(); 24])
                .join("\n")
                .contains("No action needed. Ongoing work is in Updates.")
        );
        assert!(!project_bar(&deck, 100).line.contains("\u{1b}[1;33m"));
        deck.handle_overlay_key(&Key::Right);
        let workspace = deck.slots[0].workspace_id;
        deck.apply_attention(workspace, Err("unavailable".into()));
        assert!(!project_bar(&deck, 100).line.contains("! Action needed"));
        let frame = render_overlay(&deck, 24, 100, &vec![String::new(); 24]).join("\n");
        assert!(frame.contains("Status unknown for 1 project"));
        assert!(!frame.contains("Running"));
        deck.handle_overlay_key(&Key::Left);
        assert!(
            render_overlay(&deck, 24, 100, &vec![String::new(); 24])
                .join("\n")
                .contains("No confirmed actions")
        );
    }

    #[test]
    fn observation_rounds_do_not_starve_projects_beyond_sixteen() {
        let mut deck = deck();
        let template = deck.slots[0].clone();
        for _ in 0..20 {
            let mut slot = template.clone();
            slot.workspace_id = WorkspaceId::new();
            deck.slots.push(slot);
        }
        let first = deck.attention_workspaces();
        let second = deck.attention_workspaces();
        assert_eq!(first.len(), 16);
        assert_eq!(second.len(), 16);
        let all = first.into_iter().chain(second).collect::<HashSet<_>>();
        assert_eq!(all.len(), 22);
        deck.slots.clear();
        assert!(deck.attention_workspaces().is_empty());
    }
}

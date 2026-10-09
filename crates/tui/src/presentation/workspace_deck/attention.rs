//! Process-level attention list. Snapshot failures never become zero work.
use super::{
    AttentionDestination, DeckOverlay, HashSet, Key, OverlayIntent, Path, PathBuf, Role, SessionId,
    WorkspaceDeck, WorkspaceId, WorkspaceSlot, fuzzy_score, modal, widgets,
};
use usagi_core::domain::{
    attention::WorkspaceAttention, presentation_text::sanitize_presentation_line,
};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct AttentionList {
    selected: Option<(WorkspaceId, String)>,
    filter: String,
}

struct Row {
    identity: (WorkspaceId, String),
    label: String,
    intent: OverlayIntent,
    stale: bool,
    rank: u8,
}

fn rows(slots: &[WorkspaceSlot], filter: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    for slot in slots {
        let project = sanitize_presentation_line(&slot.label);
        if let Some(snapshot) = &slot.attention {
            for item in &snapshot.items {
                let destination =
                    if item.kind == usagi_core::domain::attention::AttentionKind::Decision {
                        AttentionDestination::Decisions
                    } else if item.key.starts_with("pr:")
                        || item.kind == usagi_core::domain::attention::AttentionKind::Review
                    {
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
                    label: format!(
                        "{} / {} · {} · {}{}",
                        project,
                        sanitize_presentation_line(&item.label),
                        item.kind.label(),
                        sanitize_presentation_line(&item.reason),
                        if slot.attention_stale { " [Stale]" } else { "" }
                    ),
                    intent,
                    stale: slot.attention_stale,
                    rank: match item.kind {
                        usagi_core::domain::attention::AttentionKind::Decision => 0,
                        usagi_core::domain::attention::AttentionKind::Review => 1,
                        usagi_core::domain::attention::AttentionKind::Blocked => 2,
                        usagi_core::domain::attention::AttentionKind::System => 3,
                        usagi_core::domain::attention::AttentionKind::Running => 4,
                    },
                });
            }
        }
        if slot.attention.is_none() || slot.attention_stale {
            rows.push(Row {
                identity: (slot.workspace_id, "unavailable".into()),
                label: format!(
                    "{project} · {}",
                    if slot.attention_stale {
                        "Status unavailable — retrying"
                    } else {
                        "Loading status…"
                    }
                ),
                intent: OverlayIntent::Stay,
                stale: true,
                rank: 0,
            });
        }
    }
    rows.retain(|row| filter.is_empty() || fuzzy_score(filter, &row.label).is_some());
    rows.sort_by_key(|row| row.rank);
    rows
}

impl AttentionList {
    fn index(&self, rows: &[Row]) -> usize {
        self.selected
            .as_ref()
            .and_then(|selected| rows.iter().position(|row| &row.identity == selected))
            .unwrap_or(0)
    }

    pub(super) fn handle(&mut self, key: &Key, slots: &[WorkspaceSlot]) -> OverlayIntent {
        let rows = rows(slots, &self.filter);
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
                        !row.stale
                            && self
                                .selected
                                .as_ref()
                                .is_none_or(|selected| *selected == row.identity)
                    })
                    .map_or(OverlayIntent::Stay, |row| row.intent.clone());
            }
            Key::Escape => return OverlayIntent::Cancel,
            Key::Char(character) => {
                self.filter.push(*character);
                self.selected = None;
                return OverlayIntent::Stay;
            }
            Key::Paste(text) => {
                self.filter.push_str(&sanitize_presentation_line(text));
                self.selected = None;
                return OverlayIntent::Stay;
            }
            Key::Backspace => {
                self.filter.pop();
                self.selected = None;
                return OverlayIntent::Stay;
            }
            _ => index,
        };
        self.selected = rows.get(next).map(|row| row.identity.clone());
        OverlayIntent::Stay
    }
}

pub(super) fn badge(slot: &WorkspaceSlot) -> String {
    match &slot.attention {
        _ if slot.attention_stale => " !?".into(),
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
        let selected = rows(&self.slots, "")
            .first()
            .map(|row| row.identity.clone());
        self.overlay = Some(DeckOverlay::Attention(AttentionList {
            selected,
            filter: String::new(),
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
        previous != (slot.attention.clone(), slot.attention_stale)
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

pub(super) fn render(
    deck: &WorkspaceDeck,
    list: &AttentionList,
    height: usize,
    width: usize,
    base: &[String],
) -> Vec<String> {
    let inner = modal::modal_inner_width(width, 110);
    let visible = rows(&deck.slots, &list.filter);
    let selected = list.index(&visible);
    let mut action = 0;
    let mut system = 0;
    let mut running = 0;
    let mut unknown = 0;
    for slot in &deck.slots {
        let Some(snapshot) = slot.attention.as_ref().filter(|_| !slot.attention_stale) else {
            unknown += 1;
            continue;
        };
        for item in &snapshot.items {
            if item.kind.needs_action() {
                action += 1;
            } else if item.kind == usagi_core::domain::attention::AttentionKind::System {
                system += 1;
            } else {
                running += 1;
            }
        }
    }
    let mut body = vec![
        format!(
            "  Action needed: {action} · System wait: {system} · Running: {running} · Unknown projects: {unknown}"
        ),
        modal::filter_line(&list.filter, list.filter.len(), None),
    ];
    let rendered = visible
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let text = format!(
                "  {} {}",
                modal::selection_marker(index == selected),
                row.label
            );
            let text = widgets::clip_to_width(&text, inner);
            if index == selected {
                Role::Accent.style().bold().paint(&text)
            } else {
                text
            }
        })
        .collect::<Vec<_>>();
    if rendered.is_empty() {
        body.push("  No matching work".into());
    }
    let (start, end) = modal::list_window(
        rendered.len(),
        selected,
        height.saturating_sub(10).clamp(1, 14),
    );
    body.extend(modal::scroll_window(&rendered, start, end));
    if let Some(notice) = deck.notice() {
        body.push(modal::error_line(notice, inner));
    }
    body.push(modal::footer(
        "type filter / ↑↓ select / Enter visit / Esc close",
    ));
    modal::render_body_over(
        height,
        width,
        base,
        "All projects · Attention",
        inner,
        height.saturating_sub(2).min(22),
        body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::workspace_deck::{
        ProjectBarTarget, Workspace, WorkspaceSnapshot, project_bar, render_overlay,
    };
    use chrono::Utc;
    use usagi_core::domain::attention::{AttentionItem, AttentionKind};

    fn deck() -> WorkspaceDeck {
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
    fn item(key: &str, kind: AttentionKind) -> AttentionItem {
        AttentionItem {
            key: key.into(),
            session: Some(SessionId::new()),
            label: "build".into(),
            kind,
            reason: "needs a choice".into(),
        }
    }
    fn update(deck: &mut WorkspaceDeck, index: usize, items: Vec<AttentionItem>) {
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
        assert_eq!(badge(&deck.slots[1]), " !?");
        assert_eq!(deck.handle_overlay_key(&Key::Enter), OverlayIntent::Stay);
        let frame = render_overlay(&deck, 24, 120, &vec![String::new(); 24]).join("\n");
        assert!(frame.contains("[Stale]"));
        assert!(frame.contains("Unknown projects: 1"));
        assert!(frame.contains("Action needed: 0"));
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
        assert!(frame.contains("No matching work"));
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
        assert!(frame.contains("System wait: 1"));
        assert!(frame.contains("Running: 1"));
        let small = render_overlay(&deck, 5, 12, &vec![String::new(); 5]);
        assert_eq!(small.len(), 5);
    }

    #[test]
    fn attention_expires_and_pending_navigation_is_consumed_only_at_its_destination() {
        let mut deck = deck();
        assert!(!deck.expire_attention(std::time::Instant::now()));
        update(&mut deck, 0, vec![item("ci", AttentionKind::System)]);
        let later = std::time::Instant::now() + std::time::Duration::from_secs(31);
        assert!(deck.expire_attention(later));
        assert!(!deck.expire_attention(later));
        assert_eq!(badge(&deck.slots[0]), " !?");
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
        assert!(frame.contains("Loading"));
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
        assert!(frame.contains("Action needed: 2"));
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

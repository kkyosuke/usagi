use super::*;
use crate::presentation::widgets::display_width;
use crate::presentation::workspace_deck::{render_overlay, tests::snapshot_with_sessions};
use crate::usecase::application::pane::TabSelection;
use usagi_core::domain::id::OperationId;

fn deck() -> WorkspaceDeck {
    WorkspaceDeck::from_snapshots(&[
        snapshot_with_sessions("same", "/alpha", &["日本語", "duplicate"]),
        snapshot_with_sessions("same", "/beta", &["duplicate"]),
    ])
    .unwrap()
}

#[test]
fn search_disambiguates_context_and_reuses_available_commands() {
    let mut deck = deck();
    let workspace = deck.slots[0].workspace_id;
    let first = deck.slots[0].sessions[0].projected.id;
    let failed = deck.slots[0].sessions[1].projected.id;
    deck.slots[0].sessions[1].projected.lifecycle = SessionLifecycle::Failed;
    for session in [first, failed, SessionId::new()] {
        deck.slots[0].palette_tabs.push(CachedTab {
            action: Action::Tab {
                target: Target::Session(session),
                selection: TabSelection::Pending(OperationId::new()),
                kind: PaneKind::Agent,
            },
            label: "Tab · Agent".into(),
        });
    }
    deck.slots[0].palette_tabs.push(CachedTab {
        action: Action::Tab {
            target: Target::Root(workspace),
            selection: TabSelection::Pending(OperationId::new()),
            kind: PaneKind::Terminal,
        },
        label: "Tab · Terminal".into(),
    });
    let found = rows(&deck.slots, "COMMAND TERMINAL 日本語 alpha");
    assert_eq!(found.len(), 1);
    assert_eq!(
        found[0].identity.action,
        Action::Command {
            session: Some(first),
            name: "terminal"
        }
    );
    assert!(found[0].context.contains("/alpha"));
    assert!(rows(&deck.slots, "unmatchable-xyz").is_empty());
    let all = rows(&deck.slots, "");
    assert!(
        !all.iter()
            .any(|row| row.identity.action.session() == Some(failed))
    );
    assert!(
        !all.iter()
            .any(|row| matches!(row.identity.action, Action::Command { name: "diff", .. }))
    );
    assert_eq!(
        all.iter()
            .filter(|row| matches!(row.identity.action, Action::Tab { .. }))
            .count(),
        2
    );
    assert!(all.iter().any(|row| row.context.contains("/beta")));
}

#[test]
fn disappearing_selection_never_falls_through_to_a_same_name_replacement() {
    let mut deck = deck();
    deck.open_command_palette();
    deck.handle_overlay_key(&Key::Paste("Session duplicate beta".into()));
    let old = deck.slots[1].sessions[0].projected.id;
    let selected = deck.handle_overlay_key(&Key::Enter);
    assert!(
        matches!(selected, OverlayIntent::PaletteVisit { action: Action::Session(id), .. } if id == old)
    );
    deck.slots[1].sessions[0].projected.id = SessionId::new();
    assert_eq!(deck.handle_overlay_key(&Key::Enter), OverlayIntent::Stay);
    let frame = render_overlay(&deck, 24, 100, &[]).join("\n");
    assert!(frame.contains("no longer available"));
    deck.handle_overlay_key(&Key::Down);
    assert!(
        matches!(deck.handle_overlay_key(&Key::Enter), OverlayIntent::PaletteVisit { action: Action::Session(id), .. } if id != old)
    );
    deck.open_command_palette();
    deck.slots.clear();
    assert_eq!(deck.handle_overlay_key(&Key::Enter), OverlayIntent::Stay);
    deck.handle_overlay_key(&Key::Up);
    assert_eq!(deck.handle_overlay_key(&Key::Enter), OverlayIntent::Stay);
}

#[test]
fn query_editing_is_bounded_unicode_aware_and_swallows_unhandled_keys() {
    let deck = deck();
    let mut palette = Palette::default();
    for key in [
        Key::Paste("日e\u{301}本".into()),
        Key::Home,
        Key::Right,
        Key::Delete,
        Key::End,
        Key::Backspace,
        Key::Char('語'),
        Key::Left,
        Key::LineStart,
        Key::LineEnd,
    ] {
        assert_eq!(palette.handle(&key, &deck.slots, 3), OverlayIntent::Stay);
    }
    assert_eq!(palette.input.value(), "日語");
    palette.handle(&Key::Paste("x".repeat(300)), &deck.slots, 3);
    palette.handle(&Key::Char('y'), &deck.slots, 3);
    assert_eq!(palette.input.value().chars().count(), MAX_QUERY_CHARS);
    assert_eq!(
        palette.handle(&Key::Enter, &deck.slots, 3),
        OverlayIntent::Stay
    );
    assert_eq!(
        palette.handle(&Key::Other, &deck.slots, 3),
        OverlayIntent::Stay
    );
    for key in [Key::Escape, Key::Quit] {
        assert_eq!(palette.handle(&key, &deck.slots, 3), OverlayIntent::Cancel);
    }
    palette = Palette::default();
    palette.select_first(&deck.slots);
    let first = palette.selected.clone();
    palette.handle(&Key::PageDown, &deck.slots, 3);
    assert_ne!(palette.selected, first);
    palette.handle(&Key::PageUp, &deck.slots, 3);
    assert_eq!(palette.selected, first);
}

#[test]
fn rendering_follows_selection_and_fits_short_narrow_screens() {
    let mut deck = deck();
    deck.open_command_palette();
    for _ in 0..6 {
        deck.handle_overlay_key_at(&Key::PageDown, 24, 90);
    }
    for (height, width) in [(24, 90), (10, 30), (8, 10), (1, 1)] {
        let frame = render_overlay(&deck, height, width, &[]);
        assert_eq!(frame.len(), height);
        assert!(frame.iter().all(|line| display_width(line) <= width));
        if height >= 10 {
            assert!(frame.join("\n").contains("Enter open"));
        }
    }
    deck.set_notice("Target unavailable");
    assert!(
        render_overlay(&deck, 24, 90, &[])
            .join("\n")
            .contains("Target unavailable")
    );
    deck.handle_overlay_key(&Key::Paste("unmatchable-xyz".into()));
    assert!(
        render_overlay(&deck, 24, 90, &[])
            .join("\n")
            .contains("No matching")
    );
    assert_eq!(page_size(0), 1);
    assert_eq!(page_size(24), 10);
}

#[test]
fn metadata_and_pending_visits_are_scoped_to_workspace_incarnations() {
    let mut deck = deck();
    let workspace = deck.slots[0].workspace_id;
    let path = deck.slots[0].path.clone();
    let mut runtime = WorkspaceRuntime::new(workspace, vec![]);
    runtime.request_pane(
        Target::Root(workspace),
        OperationId::new(),
        PaneKind::Terminal,
    );
    deck.update_palette_tabs(&runtime);
    assert_eq!(deck.slots[0].palette_tabs.len(), 1);
    deck.update_palette_tabs(&WorkspaceRuntime::new(WorkspaceId::new(), vec![]));
    assert_eq!(deck.slots[0].palette_tabs.len(), 1);
    deck.schedule_palette_visit(path.clone(), workspace, Action::Workspace);
    assert_eq!(deck.take_palette_visit(Path::new("/other")), None);
    assert_eq!(
        deck.take_palette_visit(&path),
        Some((workspace, Action::Workspace))
    );
    assert_eq!(deck.take_palette_visit(&path), None);
    deck.schedule_palette_visit(path.clone(), workspace, Action::Workspace);
    deck.activate_snapshot(&snapshot_with_sessions("new", "/alpha", &[]));
    assert!(deck.slots[0].palette_tabs.is_empty());
    deck.close_path(&path);
    assert_eq!(deck.take_palette_visit(&path), None);
}

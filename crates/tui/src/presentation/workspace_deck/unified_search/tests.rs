use super::*;
use crate::presentation::workspace_deck::{render_overlay, tests::snapshot_with_sessions};
use crate::usecase::application::{
    knowledge_search::{Kind, Results as Knowledge},
    unified_search::Results,
};
use usagi_core::domain::id::{DaemonGeneration, TerminalId, TerminalRef, WorktreeId};
use usagi_core::usecase::{
    terminal_search::{
        SearchBudget, SearchTerminalKind, TerminalSearchResult, WorkspaceOutputSearch,
    },
    vt_screen::VtScreen,
};

fn deck() -> WorkspaceDeck {
    WorkspaceDeck::new(&snapshot_with_sessions(
        "project",
        "/project",
        &["one", "two"],
    ))
}
fn search(deck: &WorkspaceDeck) -> &Search {
    let Some(DeckOverlay::Unified(search)) = &deck.overlay else {
        panic!()
    };
    search
}
fn results(deck: &WorkspaceDeck) -> Results {
    let slot = &deck.slots[0];
    let mut output = TerminalSearchResult::unavailable(
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            workspace_id: slot.workspace_id,
            session_id: Some(slot.sessions[0].projected.id),
            worktree_id: WorktreeId::new(),
            terminal_id: TerminalId::new(),
        },
        SearchTerminalKind::Terminal,
    );
    let mut screen = VtScreen::new(2, 40);
    screen.advance(b"zzzxmatch");
    output.search(
        &screen.checkpoint(),
        "zzzxmatch",
        &mut SearchBudget::default(),
    );
    Results {
        outputs: vec![(
            slot.workspace_id,
            Ok(WorkspaceOutputSearch {
                workspace: slot.workspace_id,
                session: None,
                query: "zzzxmatch".into(),
                terminals: vec![output],
                omitted_terminals: 0,
            }),
        )],
        artifacts: vec![Artifact {
            workspace: slot.workspace_id,
            project_root: slot.path.clone(),
            target: Target::Session(slot.sessions[0].projected.id),
            path: "artifacts/zzzxmatch.txt".into(),
        }],
        knowledge: Knowledge {
            documents: vec![Document {
                kind: Kind::Memory,
                path: ".usagi/memory/exact.md".into(),
                title: "zzzxmatch title".into(),
                detail: "detail".into(),
            }],
            omitted: 2,
            errors: vec!["partial failure".into()],
        },
        omitted: 3,
        ..Results::default()
    }
}
fn complete(deck: &mut WorkspaceDeck, result: Results) -> (u64, u64) {
    let (token, _) = deck.take_unified_request().unwrap();
    assert!(deck.apply_unified_response(token, Response::Search(result)));
    token
}
#[test]
fn unified_search_scopes_preserve_query_current_identity_and_exact_activation() {
    let mut deck = deck();
    assert_eq!(deck.unified_search_token(), None);
    assert!(deck.take_unified_request().is_none());
    assert!(!deck.apply_unified_response((0, 0), Response::Search(Results::default())));
    let session = deck.slots[0].sessions[0].projected.id;
    deck.open_unified_search(Some(session));
    assert!(matches!(
        deck.handle_overlay_key(&Key::Enter),
        OverlayIntent::PaletteVisit { .. }
    ));
    let (old, _) = deck.take_unified_request().unwrap();
    deck.handle_overlay_key(&Key::Paste("zzzxmatch".into()));
    let result = results(&deck);
    assert!(!deck.apply_unified_response(old, Response::Search(result.clone())));
    let token = complete(&mut deck, result.clone());
    assert_eq!(deck.unified_search_token(), Some(token));
    assert!(!deck.apply_unified_response(token, Response::Search(result.clone())));
    assert!(deck.take_unified_request().is_none());
    assert_eq!(search(&deck).entries.len(), 3);
    let frame = render_overlay(&deck, 35, 140, &[]).join("\n");
    for text in [
        "[All]",
        "zzzxmatch",
        "5 omitted",
        "partial failure",
        "artifacts/zzzxmatch.txt",
    ] {
        assert!(frame.contains(text), "{text}: {frame}");
    }
    assert!(matches!(
        deck.handle_overlay_key(&Key::Enter),
        OverlayIntent::OutputVisit(_)
    ));
    deck.handle_overlay_key(&Key::Down);
    assert!(
        matches!(deck.handle_overlay_key(&Key::Enter), OverlayIntent::PaletteVisit { action: palette::Action::Artifact { path, .. }, .. } if path == "artifacts/zzzxmatch.txt")
    );
    for scope in [
        Scope::Terminal,
        Scope::Artifact,
        Scope::CurrentSession,
        Scope::All,
    ] {
        deck.handle_overlay_key(&Key::Tab);
        assert_eq!(search(&deck).scope, scope);
        assert_eq!(search(&deck).input.value(), "zzzxmatch");
        assert_eq!(search(&deck).current.as_ref().unwrap().session, session);
        complete(&mut deck, result.clone());
        assert_eq!(
            search(&deck).entries.len(),
            match scope {
                Scope::All => 3,
                Scope::CurrentSession => 2,
                _ => 1,
            }
        );
    }
    deck.handle_overlay_key(&Key::Passthrough(b"\x1b[Z".to_vec()));
    assert_eq!(search(&deck).scope, Scope::CurrentSession);
    deck.open_unified_search(None);
    assert_eq!(search(&deck).current.as_ref().unwrap().session, session);
    assert!(
        render_overlay(&deck, 28, 120, &[])
            .join("\n")
            .contains("Current session:")
    );
    deck.close_overlay();
    deck.open_unified_search(None);
    for _ in 0..3 {
        deck.handle_overlay_key(&Key::Tab);
    }
    assert!(search(&deck).entries.is_empty());
    assert!(
        render_overlay(&deck, 28, 120, &[])
            .join("\n")
            .contains("No current session")
    );
    complete(&mut deck, result);
    assert!(search(&deck).entries.is_empty());
    assert!(!deck.apply_unified_response(token, Response::Search(Results::default())));
}

#[test]
fn unified_search_document_reader_fences_loading_back_scope_switch_and_reopen() {
    let mut deck = deck();
    deck.open_unified_search(None);
    deck.handle_overlay_key(&Key::Paste("zzzxmatch".into()));
    let result = results(&deck);
    complete(&mut deck, result);
    deck.handle_overlay_key(&Key::PageDown);
    assert!(matches!(search(&deck).selected, Some(Entry::Document(_))));
    deck.handle_overlay_key(&Key::Enter);
    let (token, request) = deck.take_unified_request().unwrap();
    assert_eq!(request, Request::Read(".usagi/memory/exact.md".into()));
    assert!(!deck.apply_unified_response(token, Response::Search(Results::default())));
    assert!(deck.apply_unified_response(
        token,
        Response::Read(Ok(vec![
            "before".into(),
            "ZZZXMATCH body".into(),
            "after".into()
        ]))
    ));
    assert_eq!(search(&deck).scroll, 1);
    assert!(
        render_overlay(&deck, 30, 140, &[])
            .join("\n")
            .contains("2: ZZZXMATCH body")
    );
    for key in [
        Key::Down,
        Key::PageDown,
        Key::Up,
        Key::PageUp,
        Key::End,
        Key::Home,
        Key::Right,
        Key::Left,
        Key::Paste("ignored".into()),
    ] {
        deck.handle_overlay_key(&key);
    }
    assert_eq!(search(&deck).input.value(), "zzzxmatch");
    deck.handle_overlay_key(&Key::Escape);
    assert!(deck.take_unified_request().is_none());
    assert!(!deck.apply_unified_response(token, Response::Read(Ok(vec![]))));
    deck.handle_overlay_key(&Key::Enter);
    let (token, _) = deck.take_unified_request().unwrap();
    assert!(deck.apply_unified_response(token, Response::Read(Err("Source disappeared".into()))));
    assert!(
        render_overlay(&deck, 30, 140, &[])
            .join("\n")
            .contains("Source disappeared")
    );
    assert_eq!(deck.handle_overlay_key(&Key::Quit), OverlayIntent::Cancel);
    deck.handle_overlay_key(&Key::Escape);
    deck.handle_overlay_key(&Key::Enter);
    let (token, _) = deck.take_unified_request().unwrap();
    assert!(deck.apply_unified_response(token, Response::Read(Ok(vec!["no match".into()]))));
    assert_eq!(search(&deck).scroll, 0);
    deck.handle_overlay_key(&Key::Tab);
    assert!(search(&deck).document.is_none());
    deck.close_overlay();
    deck.open_unified_search(None);
    assert!(!deck.apply_unified_response(token, Response::Read(Err("stale".into()))));
}

#[test]
fn unified_search_input_bounds_selection_small_frames_and_refresh() {
    let mut deck = deck();
    deck.open_unified_search(None);
    assert!(
        render_overlay(&deck, 30, 140, &[])
            .join("\n")
            .contains("Type a query")
    );
    for key in [
        Key::Char('a'),
        Key::Left,
        Key::Delete,
        Key::Char('b'),
        Key::Home,
        Key::Right,
        Key::End,
        Key::Backspace,
        Key::LineStart,
        Key::LineEnd,
        Key::Up,
        Key::Down,
        Key::PageUp,
        Key::PageDown,
        Key::Passthrough(vec![3]),
    ] {
        deck.handle_overlay_key(&key);
    }
    assert_eq!(search(&deck).input.value(), "");
    deck.handle_overlay_key(&Key::Paste(format!("\n\r{}", "語".repeat(300))));
    deck.handle_overlay_key(&Key::Char('x'));
    assert_eq!(search(&deck).input.value().chars().count(), QUERY_LIMIT);
    let (old, _) = deck.take_unified_request().unwrap();
    deck.open_unified_search(None);
    assert_ne!(deck.unified_search_token(), Some(old));
    let (token, _) = deck.take_unified_request().unwrap();
    assert!(!deck.apply_unified_response(token, Response::Read(Ok(vec![]))));
    assert!(deck.apply_unified_response(token, Response::Search(Results::default())));
    assert_eq!(deck.handle_overlay_key(&Key::Enter), OverlayIntent::Stay);
    deck.set_notice("target disappeared");
    let frame = render_overlay(&deck, 30, 140, &[]).join("\n");
    assert!(frame.contains("No matching results") && frame.contains("target disappeared"));
    for (h, w) in [(1, 1), (5, 8), (12, 25)] {
        assert_eq!(render_overlay(&deck, h, w, &[]).len(), h);
    }
    assert_eq!(deck.handle_overlay_key(&Key::Escape), OverlayIntent::Cancel);
    assert_eq!(deck.handle_overlay_key(&Key::Quit), OverlayIntent::Cancel);
    assert_eq!(deck.clone(), deck);
    assert!(format!("{deck:?}").contains("Unified"));
}

#[test]
fn unified_search_current_navigation_updates_keep_selected_identity_and_source_rows() {
    let mut deck = deck();
    assert!(!deck.refresh_unified_navigation());
    let session = deck.slots[0].sessions[0].projected.id;
    deck.open_unified_search(Some(session));
    for _ in 0..3 {
        deck.handle_overlay_key(&Key::Tab);
    }
    assert!(!search(&deck).entries.is_empty());
    assert!(search(&deck).entries.iter().all(|entry| matches!(entry, Entry::Navigation(row) if row.identity.action.session() == Some(session))));
    assert!(!deck.refresh_unified_navigation());
    let selected = search(&deck).selected.clone();
    deck.slots[0].sessions.clear();
    assert!(deck.refresh_unified_navigation());
    assert_eq!(search(&deck).selected, selected);
    assert_eq!(deck.handle_overlay_key(&Key::Enter), OverlayIntent::Stay);

    let mut deck = self::deck();
    deck.open_unified_search(None);
    deck.handle_overlay_key(&Key::Paste("zzzxmatch".into()));
    let result = results(&deck);
    complete(&mut deck, result);
    let selected = search(&deck).selected.clone();
    assert!(!deck.refresh_unified_navigation());
    assert_eq!(search(&deck).selected, selected);
    deck.handle_overlay_key(&Key::Down);
    assert!(
        render_overlay(&deck, 30, 140, &[])
            .join("\n")
            .contains("Enter opens the artifact viewer")
    );
    deck.handle_overlay_key(&Key::Down);
    assert!(
        render_overlay(&deck, 30, 140, &[])
            .join("\n")
            .contains(".usagi/memory/exact.md")
    );
    deck.handle_overlay_key(&Key::Enter);
    assert!(!deck.refresh_unified_navigation());
}

#[test]
fn unified_search_uses_real_leader_and_backtab_input_classification() {
    use crate::infrastructure::live_input::classify_terminal_input;
    use crate::usecase::terminal_input::{
        KeyCode, KeyEvent, KeyEventKind, LiveInput, LiveInputClassifier, LiveTerminalAction,
        Modifiers,
    };
    let mut classifier = LiveInputClassifier::default();
    let now = std::time::Duration::ZERO;
    let event = |code, modifiers| {
        LiveInput::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            raw_bytes: Vec::new(),
        })
    };
    let control = Modifiers {
        control: true,
        ..Modifiers::default()
    };
    assert!(
        classify_terminal_input(&mut classifier, now, &event(KeyCode::Char('o'), control))
            .is_none()
    );
    assert_eq!(
        classify_terminal_input(&mut classifier, now, &event(KeyCode::Char('f'), control)),
        Some(Key::Live(LiveTerminalAction::OpenUnifiedSearch))
    );
    let mut deck = deck();
    deck.open_unified_search(None);
    for modifiers in [
        Modifiers::default(),
        Modifiers {
            shift: true,
            ..Modifiers::default()
        },
    ] {
        let key =
            classify_terminal_input(&mut classifier, now, &event(KeyCode::BackTab, modifiers))
                .unwrap();
        deck.handle_overlay_key(&key);
    }
    assert_eq!(search(&deck).scope, Scope::Artifact);
}

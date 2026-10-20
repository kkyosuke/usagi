use super::*;
use crate::presentation::workspace_deck::{render_overlay, tests::snapshot_with_sessions};

fn deck() -> WorkspaceDeck {
    WorkspaceDeck::new(&snapshot_with_sessions("project", "/project", &["session"]))
}
fn results() -> Results {
    Results {
        documents: vec![
            Document {
                kind: Kind::Issue,
                path: ".usagi/issues/001-exact.md".into(),
                title: "#1 issue".into(),
                detail: "todo · ready".into(),
            },
            Document {
                kind: Kind::Memory,
                path: ".usagi/memory/noncanonical.md".into(),
                title: "A memory".into(),
                detail: "feedback".into(),
            },
        ],
        omitted: 3,
        errors: vec!["partial failure".into()],
    }
}
fn search(deck: &WorkspaceDeck) -> &Search {
    let Some(DeckOverlay::Knowledge(search)) = &deck.overlay else {
        panic!()
    };
    search
}

#[test]
fn knowledge_search_preserves_exact_sources_and_fences_query_close_and_document_responses() {
    let mut deck = deck();
    assert_eq!(deck.knowledge_search_token(), None);
    assert!(deck.take_knowledge_request().is_none());
    assert!(!deck.apply_knowledge_response((0, 0), Response::Search(results())));
    deck.open_knowledge_search();
    let (old, _) = deck.take_knowledge_request().unwrap();
    assert_eq!(deck.knowledge_search_token(), Some(old));
    deck.handle_overlay_key(&Key::Paste("needle".into()));
    let (token, request) = deck.take_knowledge_request().unwrap();
    assert_eq!(deck.knowledge_search_token(), Some(token));
    assert_eq!(request, Request::Search("needle".into()));
    assert!(deck.take_knowledge_request().is_none());
    assert!(!deck.apply_knowledge_response(old, Response::Search(results())));
    assert!(!deck.apply_knowledge_response(token, Response::Read(Ok(vec![]))));
    assert!(deck.apply_knowledge_response(token, Response::Search(results())));
    let frame = render_overlay(&deck, 35, 140, &[]).join("\n");
    for text in [
        "Search Issue / Memory",
        "2 results",
        "3 omitted",
        "partial failure",
        "Workspace: /project",
        "#1 issue",
    ] {
        assert!(frame.contains(text), "{frame}");
    }
    deck.handle_overlay_key(&Key::Tab);
    assert_eq!(search(&deck).rows().len(), 1);
    deck.handle_overlay_key(&Key::Tab);
    deck.handle_overlay_key(&Key::Enter);
    let (read_token, request) = deck.take_knowledge_request().unwrap();
    assert_eq!(
        request,
        Request::Read(".usagi/memory/noncanonical.md".into())
    );
    assert!(!deck.apply_knowledge_response(token, Response::Search(results())));
    assert!(!deck.apply_knowledge_response(read_token, Response::Search(results())));
    assert!(deck.apply_knowledge_response(
        read_token,
        Response::Read(Ok(vec![
            "before".into(),
            "NEEDLE body".into(),
            "after".into()
        ]))
    ));
    assert_eq!(search(&deck).scroll, 1);
    let frame = render_overlay(&deck, 30, 140, &[]).join("\n");
    assert!(frame.contains("2: NEEDLE body"));
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
        Key::Char('q'),
    ] {
        deck.handle_overlay_key(&key);
    }
    assert_eq!(search(&deck).input.value(), "needle");
    deck.handle_overlay_key(&Key::Escape);
    assert!(deck.take_knowledge_request().is_none());
    assert!(!deck.apply_knowledge_response(read_token, Response::Read(Ok(vec![]))));
    deck.handle_overlay_key(&Key::Tab);
    assert_eq!(search(&deck).rows().len(), 2);
    deck.handle_overlay_key(&Key::Enter);
    let (token, _) = deck.take_knowledge_request().unwrap();
    assert!(deck.apply_knowledge_response(token, Response::Read(Err("Source disappeared".into()))));
    assert!(
        render_overlay(&deck, 25, 100, &[])
            .join("\n")
            .contains("Source disappeared")
    );
    assert_eq!(deck.handle_overlay_key(&Key::Quit), OverlayIntent::Cancel);
    deck.close_overlay();
    deck.open_knowledge_search();
    assert!(!deck.apply_knowledge_response(token, Response::Read(Err("stale".into()))));
    assert_eq!(deck.clone(), deck);
    assert!(format!("{deck:?}").contains("Knowledge"));
}

#[test]
fn knowledge_search_input_caps_refresh_navigation_empty_results_and_small_frames() {
    let mut deck = deck();
    deck.open_knowledge_search();
    deck.handle_overlay_key(&Key::Enter);
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
        assert_eq!(deck.handle_overlay_key(&key), OverlayIntent::Stay);
    }
    assert_eq!(search(&deck).input.value(), "");
    deck.handle_overlay_key(&Key::Paste(format!("\n\r{}", "語".repeat(300))));
    deck.handle_overlay_key(&Key::Char('x'));
    assert_eq!(search(&deck).input.value().chars().count(), QUERY_LIMIT);
    let (token, _) = deck.take_knowledge_request().unwrap();
    deck.open_knowledge_search();
    let (next, _) = deck.take_knowledge_request().unwrap();
    assert_eq!(next.0, token.0);
    assert_eq!(search(&deck).input.value().chars().count(), QUERY_LIMIT);
    assert_ne!(token, next);
    assert!(deck.apply_knowledge_response(next, Response::Search(Results::default())));
    assert!(
        render_overlay(&deck, 28, 120, &[])
            .join("\n")
            .contains("No matching documents")
    );
    assert!(deck.apply_knowledge_response(next, Response::Search(results())));
    deck.handle_overlay_key(&Key::PageDown);
    assert_eq!(search(&deck).cursor, 1);
    deck.handle_overlay_key(&Key::Up);
    deck.handle_overlay_key(&Key::Down);
    deck.handle_overlay_key(&Key::PageUp);
    assert_eq!(search(&deck).cursor, 0);
    for (height, width) in [(1, 1), (5, 8), (12, 25)] {
        assert_eq!(render_overlay(&deck, height, width, &[]).len(), height);
    }
    deck.handle_overlay_key(&Key::Enter);
    let (token, _) = deck.take_knowledge_request().unwrap();
    assert!(deck.apply_knowledge_response(token, Response::Read(Ok(vec!["no match".into()]))));
    assert_eq!(search(&deck).scroll, 0);
    deck.handle_overlay_key(&Key::Live(LiveTerminalAction::OpenKnowledgeSearch));
    assert!(search(&deck).document.is_none());
    assert_eq!(deck.handle_overlay_key(&Key::Escape), OverlayIntent::Cancel);
}

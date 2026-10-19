use super::*;
use crate::presentation::workspace_deck::{render_overlay, tests::snapshot_with_sessions};
use usagi_core::domain::id::{DaemonGeneration, TerminalId, TerminalRef, WorktreeId};
use usagi_core::usecase::{terminal_search::SearchBudget, vt_screen::VtScreen};

fn deck() -> WorkspaceDeck {
    WorkspaceDeck::new(&snapshot_with_sessions("project", "/project", &["session"]))
}

fn response(deck: &WorkspaceDeck, query: &str) -> WorkspaceOutputSearch {
    let workspace = deck.slots[0].workspace_id;
    let mut result = TerminalSearchResult::unavailable(
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            workspace_id: workspace,
            session_id: Some(deck.slots[0].sessions[0].projected.id),
            worktree_id: WorktreeId::new(),
            terminal_id: TerminalId::new(),
        },
        SearchTerminalKind::Agent,
    );
    let mut screen = VtScreen::new(3, 30);
    screen.advance(b"before\r\nerror error\r\nafter");
    result.search(&screen.checkpoint(), query, &mut SearchBudget::default());
    WorkspaceOutputSearch {
        workspace,
        query: query.into(),
        terminals: vec![result],
        omitted_terminals: 0,
    }
}

#[test]
fn output_search_fences_requeries_closure_and_workspace_identity() {
    let mut deck = deck();
    assert!(deck.take_output_search().is_none());
    assert!(!deck.apply_output_search((0, 0), vec![]));
    deck.open_output_search();
    assert!(deck.take_output_search().is_none());
    deck.handle_overlay_key(&Key::Paste("error".into()));
    let (token, query, workspaces) = deck.take_output_search().unwrap();
    assert_eq!(query, "error");
    assert!(deck.take_output_search().is_none());
    let response = response(&deck, &query);
    assert!(deck.apply_output_search(token, vec![(workspaces[0], Ok(response.clone()))]));
    let OverlayIntent::OutputVisit(hit) = deck.handle_overlay_key(&Key::Enter) else {
        panic!()
    };
    assert_eq!(hit.found.column, 0);
    assert_eq!(hit.action().session(), hit.source.terminal.session_id);
    let frame = render_overlay(&deck, 30, 120, &[]).join("\n");
    for needle in [
        "2 hits",
        "before",
        "after",
        "session",
        "Agent",
        "retained rows",
    ] {
        assert!(frame.contains(needle), "{needle}: {frame}");
    }
    deck.handle_overlay_key(&Key::Down);
    let OverlayIntent::OutputVisit(hit) = deck.handle_overlay_key(&Key::Enter) else {
        panic!()
    };
    assert_eq!(hit.found.column, 6);
    deck.handle_overlay_key(&Key::Char('x'));
    assert!(!deck.apply_output_search(token, vec![(workspaces[0], Ok(response.clone()))]));
    assert_eq!(deck.handle_overlay_key(&Key::Enter), OverlayIntent::Stay);
    deck.close_overlay();
    deck.open_output_search();
    deck.handle_overlay_key(&Key::Paste(query));
    assert!(!deck.apply_output_search(token, vec![(workspaces[0], Ok(response.clone()))]));
    let (token, _, _) = deck.take_output_search().unwrap();
    let foreign = WorkspaceId::new();
    let mut wrong_workspace = response.clone();
    wrong_workspace.workspace = foreign;
    let mut wrong_query = response.clone();
    wrong_query.query = "other".into();
    let mut bad_terminal = response.clone();
    bad_terminal.terminals[0].terminal.workspace_id = foreign;
    let mut partial = response.clone();
    partial.omitted_terminals = 2;
    partial.terminals[0].kind = SearchTerminalKind::Terminal;
    partial.terminals[0].terminal.session_id = None;
    partial.terminals[0].exited = true;
    partial.terminals[0].limited = true;
    partial.terminals.push(TerminalSearchResult::unavailable(
        partial.terminals[0].terminal.clone(),
        SearchTerminalKind::Terminal,
    ));
    assert!(deck.apply_output_search(
        token,
        vec![
            (foreign, Ok(response)),
            (workspaces[0], Err("unavailable".into())),
            (workspaces[0], Ok(wrong_workspace)),
            (workspaces[0], Ok(wrong_query)),
            (workspaces[0], Ok(bad_terminal)),
            (workspaces[0], Ok(partial)),
        ]
    ));
    let OverlayIntent::OutputVisit(hit) = deck.handle_overlay_key(&Key::Enter) else {
        panic!()
    };
    assert!(matches!(
        hit.action(),
        super::super::palette::Action::Tab {
            kind: PaneKind::Terminal,
            target: Target::Root(_),
            ..
        }
    ));
    deck.set_notice("target disappeared");
    let frame = render_overlay(&deck, 30, 120, &[]).join("\n");
    assert!(frame.contains("5 unavailable"));
    assert!(frame.contains("3 limited/omitted"));
    assert!(frame.contains("target disappeared"));
    assert!(frame.contains("exited"));
    assert!(!format!("{deck:?}").is_empty());
    assert_eq!(deck.clone(), deck);
}

#[test]
fn output_search_owns_input_bounds_paste_and_refresh() {
    let mut deck = deck();
    deck.open_output_search();
    for key in [
        Key::Char('a'),
        Key::Left,
        Key::Delete,
        Key::Char('x'),
        Key::Home,
        Key::Right,
        Key::End,
        Key::Backspace,
        Key::Paste("error\n".into()),
        Key::Up,
        Key::PageUp,
        Key::PageDown,
        Key::Passthrough(vec![3]),
        Key::Tab,
    ] {
        assert_eq!(deck.handle_overlay_key(&key), OverlayIntent::Stay);
    }
    let (first, query, _) = deck.take_output_search().unwrap();
    assert_eq!(query, "error");
    deck.handle_overlay_key(&Key::Tab);
    assert_ne!(deck.take_output_search().unwrap().0, first);
    deck.handle_overlay_key(&Key::Paste("x".repeat(QUERY_MAX * 2)));
    deck.handle_overlay_key(&Key::Char('z'));
    assert_eq!(
        deck.take_output_search().unwrap().1.chars().count(),
        QUERY_MAX
    );
    assert_eq!(deck.handle_overlay_key(&Key::Escape), OverlayIntent::Cancel);
    deck.open_output_search();
    deck.handle_overlay_key(&Key::Char('a'));
    deck.handle_overlay_key(&Key::Backspace);
    assert!(
        render_overlay(&deck, 12, 40, &[])
            .join("\n")
            .contains("Type to search")
    );
}

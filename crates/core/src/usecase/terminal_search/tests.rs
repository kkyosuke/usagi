use super::*;
use crate::domain::id::{DaemonGeneration, SessionId, TerminalId, WorktreeId};

fn result() -> TerminalSearchResult {
    TerminalSearchResult::unavailable(
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            workspace_id: WorkspaceId::new(),
            session_id: Some(SessionId::new()),
            worktree_id: WorktreeId::new(),
            terminal_id: TerminalId::new(),
        },
        SearchTerminalKind::Agent,
    )
}

#[test]
fn searches_semantic_cells_with_context_and_unicode_columns() {
    let mut screen = VtScreen::new(4, 40);
    screen.advance("before\r\n日本語 \x1b[31merror\x1b[0m error\r\nafter".as_bytes());
    let mut result = result();
    result.search(&screen.checkpoint(), "error", &mut SearchBudget::default());
    assert!(!result.unavailable);
    assert_eq!(result.matches.len(), 2);
    assert_eq!(result.matches[0].row, 1);
    assert_eq!(result.matches[0].column, 7);
    assert_eq!(result.matches[0].end_column, 11);
    assert_eq!(result.matches[0].before, "before");
    assert_eq!(result.matches[0].after, "after");
    assert_eq!(result.matches[0].text, "日本語 error error");
    assert_eq!(result.clone(), result);
    assert!(format!("{result:?}").contains("error"));
}

#[test]
fn match_columns_follow_the_terminal_cells_after_emoji_sequences() {
    let mut screen = VtScreen::new(2, 10);
    screen.advance("👍🏽 error".as_bytes());
    let mut result = result();
    result.search(&screen.checkpoint(), "error", &mut SearchBudget::default());
    assert_eq!(screen.grid()[0][5].ch(), 'e');
    assert_eq!(result.matches[0].column, 5);
    assert_eq!(result.matches[0].end_column, 9);
}

#[test]
fn invalid_queries_and_checkpoints_are_not_empty_successes() {
    for query in ["", "\n", &"a".repeat(QUERY_MAX + 1)] {
        assert!(!valid_query(query));
    }
    assert!(valid_query(&"あ".repeat(QUERY_MAX)));
    let mut result = result();
    let screen = VtScreen::new(2, 8);
    result.search(&screen.checkpoint(), "", &mut SearchBudget::default());
    assert!(result.unavailable);
    let mut invalid = screen.checkpoint();
    invalid.schema_version += 1;
    result.search(&invalid, "word", &mut SearchBudget::default());
    assert!(result.unavailable);
}

#[test]
fn shared_work_and_result_limits_are_reported() {
    let mut screen = VtScreen::new(2, 200);
    screen.advance("x".repeat(200).as_bytes());
    let mut result = result();
    result.search(&screen.checkpoint(), "x", &mut SearchBudget::default());
    assert_eq!(result.matches.len(), MATCHES_MAX);
    assert!(result.limited);
    for mut budget in [
        SearchBudget {
            rows: ROWS_MAX,
            ..SearchBudget::default()
        },
        SearchBudget {
            bytes: BYTES_MAX - 1,
            ..SearchBudget::default()
        },
    ] {
        let mut limited = super::tests::result();
        limited.search(&screen.checkpoint(), "x", &mut budget);
        assert!(limited.limited);
        assert_eq!(limited.scanned_rows, 0);
    }
}

#[test]
fn trimmed_history_and_screen_edges_are_explicit() {
    let mut screen = VtScreen::new(2, 10);
    screen.advance(b"edge\r\nedge\r\nedge");
    let mut checkpoint = screen.checkpoint();
    checkpoint.primary.scrollback_origin = 9;
    let mut result = result();
    result.search(&checkpoint, "edge", &mut SearchBudget::default());
    assert!(result.limited);
    assert_eq!(result.origin, 9);
    assert!(result.matches[0].before.is_empty());
    assert!(result.matches.last().unwrap().after.is_empty());
    assert_eq!(context(&"a".repeat(CONTEXT_MAX + 10)).len(), CONTEXT_MAX);
}

#[test]
fn match_context_keeps_late_unicode_matches_visible_without_unbounded_lines() {
    let text = format!("{}needle{}", "日本語".repeat(100), "x".repeat(200));
    let snippet = match_context(&text, "日本語".repeat(100).len(), "needle".len());
    assert!(snippet.starts_with('…') && snippet.ends_with('…'));
    assert!(snippet.contains("needle"));
    assert_eq!(snippet.chars().count(), 168);
    let mut screen = VtScreen::new(2, 5);
    screen.advance(b"ready");
    let mut result = result();
    result.search(
        &screen.checkpoint(),
        "missing",
        &mut SearchBudget::default(),
    );
    assert!(result.matches.is_empty());
    assert!(!result.unavailable);
    assert_eq!(result.scanned_rows, 2);
}

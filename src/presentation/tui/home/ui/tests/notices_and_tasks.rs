use super::*;
use crate::presentation::tui::home::tasks::TaskKind;

#[test]
fn render_frame_speaks_the_update_notice_from_the_sidebar_rabbit() {
    // A newer release makes the resting sidebar mascot speak the notice (the
    // message and the new version) from a bubble above it — no top-right banner.
    let mut state = state_with(Vec::new());
    state.set_update(crate::domain::version::Version::parse("9.9.9"));
    let joined = stripped(&render_frame(24, 100, &state));
    assert!(joined.contains("アップデートがあるぴょん"));
    assert!(joined.contains("v9.9.9"));
    // The bubble's tail points down to the mascot, so the news reads as spoken.
    assert!(joined.contains('┬'));
}

#[test]
fn render_frame_hides_the_update_notice_by_default() {
    let state = state_with(Vec::new());
    let joined = stripped(&render_frame(24, 100, &state));
    assert!(!joined.contains("アップデートがあるぴょん"));
}

#[test]
fn render_frame_hides_the_update_notice_when_the_sidebar_is_collapsed() {
    // The notice lives on the sidebar mascot, so collapsing the sidebar to the
    // rail (which shows no mascot) hides it rather than relocating it.
    let mut state = state_with(Vec::new());
    state.set_update(crate::domain::version::Version::parse("9.9.9"));
    state.set_sidebar(Sidebar::Rail);
    let joined = stripped(&render_frame(24, 100, &state));
    assert!(!joined.contains("アップデートがあるぴょん"));
}

// --- waiting notice -----------------------------------------------------

#[test]
fn waiting_notice_is_empty_without_waiting_sessions() {
    assert!(waiting_notice(0).is_empty());
}

#[test]
fn render_frame_shows_waiting_count_on_the_header() {
    // The sidebar still shows the per-session `◆` waiting icon; the header adds
    // a compact top-right summary so a waiting session is visible even when the
    // sidebar is scrolled or collapsed.
    let mut waiting = worktree(Some("fix"), false, BranchStatus::Pushed);
    waiting.path = PathBuf::from("/repo/wait");
    let mut state = state_with(vec![waiting]);
    state.apply_badges(MonitorSnapshot {
        live: [PathBuf::from("/repo/wait")].into(),
        waiting: [PathBuf::from("/repo/wait")].into(),
        ..Default::default()
    });
    let frame = render_frame(24, 100, &state);
    let header = stripped(&[frame[0].clone()]);
    assert!(header.contains(" 1 waiting"));
}

#[test]
fn render_frame_keeps_waiting_notice_while_a_create_runs() {
    use super::super::super::tasks::{TaskMark, TaskRow};
    let mut waiting = worktree(Some("fix"), false, BranchStatus::Pushed);
    waiting.path = PathBuf::from("/repo/wait");
    let mut state = state_with(vec![waiting]);
    state.apply_badges(MonitorSnapshot {
        live: [PathBuf::from("/repo/wait")].into(),
        waiting: [PathBuf::from("/repo/wait")].into(),
        ..Default::default()
    });
    // A create no longer steals the corner — it shows as an inline sidebar
    // skeleton instead — so the waiting notice keeps the top-right corner and the
    // create label is not drawn there.
    state.set_tasks(vec![TaskRow {
        kind: TaskKind::CreateSession,
        label: "作成中… main".to_string(),
        mark: TaskMark::Running(0),
    }]);
    let frame = render_frame(24, 100, &state);
    let header = stripped(&[frame[0].clone()]);
    assert!(header.contains(" 1 waiting"));
    assert!(!header.contains("作成中… main"));
}

#[test]
fn render_frame_lets_a_remove_take_the_corner_over_the_waiting_notice() {
    use super::super::super::tasks::{TaskMark, TaskRow};
    let mut waiting = worktree(Some("fix"), false, BranchStatus::Pushed);
    waiting.path = PathBuf::from("/repo/wait");
    let mut state = state_with(vec![waiting]);
    state.apply_badges(MonitorSnapshot {
        live: [PathBuf::from("/repo/wait")].into(),
        waiting: [PathBuf::from("/repo/wait")].into(),
        ..Default::default()
    });
    // A removal has no inline skeleton, so it still rides the corner and hides
    // the waiting notice while it runs.
    state.set_tasks(vec![TaskRow {
        kind: TaskKind::RemoveSession,
        label: "削除中… old".to_string(),
        mark: TaskMark::Running(0),
    }]);
    let frame = render_frame(24, 100, &state);
    let header = stripped(&[frame[0].clone()]);
    assert!(header.contains("削除中… old"));
    assert!(!header.contains(" 1 waiting"));
}

// --- background-task panel ---------------------------------------------

#[test]
fn task_status_line_shows_the_running_lead_its_bar_and_count() {
    use super::super::super::tasks::{TaskMark, TaskRow};
    // A batch with one task still running and two finished: the line leads with
    // the running task (its spinner), counts two of three done, and draws a
    // partial bar.
    let rows = vec![
        TaskRow {
            kind: TaskKind::RemoveSession,
            label: "削除完了 feat".to_string(),
            mark: TaskMark::Done(true),
        },
        TaskRow {
            kind: TaskKind::CreateSession,
            label: "作成中… main".to_string(),
            mark: TaskMark::Running(0),
        },
        TaskRow {
            kind: TaskKind::CreateSession,
            label: "作成失敗 dup".to_string(),
            mark: TaskMark::Done(false),
        },
    ];
    let line = task_status_line(&rows, 100);
    // Two rows: the label on the first, the bar and count on the second.
    assert_eq!(line.len(), 2);
    // The first still-running task is the representative label, on row 1.
    assert!(stripped(&[line[0].clone()]).contains("作成中… main"));
    let plain = stripped(&line);
    // Two of the three tasks have finished.
    assert!(plain.contains("2/3"));
    // A partial bar: started but not full.
    assert!(plain.contains('[') && plain.contains('>') && plain.contains(']'));
}

#[test]
fn task_status_line_settles_on_the_last_result_once_all_finish() {
    use super::super::super::tasks::{TaskMark, TaskRow};
    // With nothing running the line falls back to the last finished task and
    // fills the bar to 2/2.
    let rows = vec![
        TaskRow {
            kind: TaskKind::RemoveSession,
            label: "作成完了 a".to_string(),
            mark: TaskMark::Done(true),
        },
        TaskRow {
            kind: TaskKind::RemoveSession,
            label: "削除完了 b".to_string(),
            mark: TaskMark::Done(true),
        },
    ];
    let plain = stripped(&task_status_line(&rows, 100));
    assert!(plain.contains("削除完了 b"));
    assert!(plain.contains("2/2"));
    // A full bar carries no `>` head: at width 100 the bar field is 17 wide.
    assert!(plain.contains(&format!("[{}]", "=".repeat(17))));
}

#[test]
fn task_status_line_leads_with_a_failure_when_the_last_task_failed() {
    use super::super::super::tasks::{TaskMark, TaskRow};
    // Nothing running and the most recent task failed: the line settles on that
    // failure (the `✗` mark) rather than an earlier success.
    let rows = vec![
        TaskRow {
            kind: TaskKind::CreateSession,
            label: "作成完了 a".to_string(),
            mark: TaskMark::Done(true),
        },
        TaskRow {
            kind: TaskKind::CreateSession,
            label: "作成失敗 dup".to_string(),
            mark: TaskMark::Done(false),
        },
    ];
    let plain = stripped(&task_status_line(&rows, 100));
    assert!(plain.contains("作成失敗 dup"));
    assert!(plain.contains('✗'));
    assert!(plain.contains("2/2"));
}

#[test]
fn task_status_line_holds_one_fixed_width_however_the_label_changes() {
    use super::super::super::tasks::{TaskMark, TaskRow};
    // A short running label and a longer finished one must produce the same
    // block width, so the right-anchored block never shifts as a row's text
    // changes. Both rows of the block also share a single width.
    let short = task_status_line(
        &[TaskRow {
            kind: TaskKind::CreateSession,
            label: "作成中… a".to_string(),
            mark: TaskMark::Running(0),
        }],
        100,
    );
    let long = task_status_line(
        &[TaskRow {
            kind: TaskKind::CreateSession,
            label: "作成完了 a-very-long-session-name".to_string(),
            mark: TaskMark::Done(true),
        }],
        100,
    );
    let row_w = |line: &[String], row: usize| console::measure_text_width(&line[row]);
    assert_eq!(row_w(&short, 0), row_w(&long, 0));
    assert_eq!(row_w(&short, 1), row_w(&long, 1));
    // Both rows of a block are the same width, so it right-aligns as a column.
    assert_eq!(row_w(&long, 0), row_w(&long, 1));
}

#[test]
fn task_status_line_is_empty_without_rows() {
    use super::super::super::tasks::TaskRow;
    let none: Vec<TaskRow> = Vec::new();
    assert!(task_status_line(&none, 100).is_empty());
}

#[test]
fn render_frame_speaks_task_status_before_the_update_notice() {
    use super::super::super::tasks::{TaskMark, TaskRow};
    let mut state = state_with(Vec::new());
    // Operational status takes the mascot bubble first: an in-flight task should
    // explain what usagi is doing now, while the update notice waits until the
    // workspace is idle.
    state.set_update(crate::domain::version::Version::parse("9.9.9"));
    state.set_tasks(vec![TaskRow {
        kind: TaskKind::CreateSession,
        label: "作成中… main".to_string(),
        mark: TaskMark::Running(0),
    }]);
    let joined = stripped(&render_frame(24, 100, &state));
    assert!(joined.contains("作成中… main"));
    assert!(!joined.contains("アップデートがあるぴょん"));
    assert!(joined.contains('┬'));
}

#[test]
fn render_frame_summarises_multiple_task_rows_in_the_mascot_bubble() {
    use super::super::super::tasks::{TaskMark, TaskRow};
    let mut state = state_with(Vec::new());
    state.set_tasks(vec![
        TaskRow {
            kind: TaskKind::RemoveSession,
            label: "作成中… main".to_string(),
            mark: TaskMark::Running(0),
        },
        TaskRow {
            kind: TaskKind::RemoveSession,
            label: "削除中… old".to_string(),
            mark: TaskMark::Running(1),
        },
    ]);
    let joined = stripped(&render_frame(24, 100, &state));
    assert!(joined.contains("作成中… main"));
    assert!(joined.contains("ほか 1 件"));
}

#[test]
fn render_frame_shows_the_task_status_on_the_header_over_a_live_terminal() {
    use super::super::super::tasks::{TaskMark, TaskRow};
    // A live embedded terminal owns the right pane (没入's attached shell, or
    // 切替's live preview). The task status now rides the header's title-bar row,
    // outside the pane, so it shows without clobbering the shell output below.
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
    state.enter_focus(1);
    state.show_attached();
    state.set_terminal_view(TerminalView::from_rows(vec!["$ echo hi".to_string()], None));
    state.set_tasks(vec![TaskRow {
        kind: TaskKind::RemoveSession,
        label: "削除中… old".to_string(),
        mark: TaskMark::Running(0),
    }]);
    let frame = render_frame(24, 100, &state);
    let joined = stripped(&frame);
    // The status shows on the header row and the shell output stays intact.
    assert!(joined.contains("削除中… old"));
    assert!(joined.contains("$ echo hi"));
    // The status rides row 0 (the title bar), not the body where the shell is.
    assert!(stripped(&[frame[0].clone()]).contains("削除中… old"));
}

#[test]
fn render_frame_speaks_the_update_from_the_sidebar_over_a_live_terminal() {
    // The update notice lives on the sidebar mascot, so it shows in the left pane
    // even while a live terminal fills the right pane — the shell output is never
    // overdrawn, and the notice stays off the header rows.
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
    state.enter_focus(1);
    state.show_attached();
    state.set_terminal_view(TerminalView::from_rows(vec!["$ echo hi".to_string()], None));
    state.set_update(crate::domain::version::Version::parse("9.9.9"));
    let frame = render_frame(24, 100, &state);
    let joined = stripped(&frame);
    assert!(joined.contains("アップデートがあるぴょん"));
    // The shell output stays intact in the body.
    assert!(joined.contains("$ echo hi"));
    // The notice is spoken from the sidebar, not the header rows (rows 0-1).
    let header = stripped(&frame[0..2]);
    assert!(!header.contains("アップデートがあるぴょん"));
}

#[test]
fn render_frame_keeps_the_run2_loading_over_a_live_terminal() {
    // The transient launch indicator is deliberate, so it still floats over the
    // right pane even while a live preview is on screen (it is painted during
    // the blocking terminal / agent spawn, before the new pane draws over the
    // screen).
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
    state.enter_focus(1);
    state.show_attached();
    state.set_terminal_view(TerminalView::from_rows(vec!["$ ".to_string()], None));
    state.step_loading("ターミナル起動中…");
    let joined = stripped(&render_frame(24, 100, &state));
    assert!(joined.contains("(｡･-･)"));
}

#[test]
fn render_frame_skips_the_big_loading_overlay_when_tabs_are_present() {
    // Pending tabs carry loading inline in their chip/body, so the legacy
    // full-pane loading overlay does not draw on top of an existing tab strip.
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
    state.enter_focus(1);
    state.set_terminal_tabs(vec!["terminal".to_string()], 0);
    state.step_loading("ターミナル起動中…");
    let joined = stripped(&render_frame(24, 100, &state));
    assert!(!joined.contains("(｡･-･)"));
}

#[test]
fn update_notice_is_skipped_when_the_sidebar_is_too_narrow_for_the_mascot() {
    // The notice rides the sidebar mascot, which is dropped when the sidebar is
    // too narrow to hold the art — so the notice goes with it rather than
    // clobbering the cramped chrome.
    let mut state = state_with(Vec::new());
    state.set_update(crate::domain::version::Version::parse("9.9.9"));
    let joined = stripped(&render_frame(24, 11, &state));
    assert!(!joined.contains("アップデートがあるぴょん"));
}

#[test]
fn render_frame_shows_the_run2_loading_while_an_action_runs() {
    let mut state = state_with(Vec::new());
    state.step_loading("削除中… 1/2");
    let joined = stripped(&render_frame(24, 100, &state));
    assert!(joined.contains("(｡･-･)"));
}

#[test]
fn run2_loading_blanks_the_focus_action_menu() {
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
    state.enter_focus(1);

    // The idle 在席 menu floats as an overlay modal composited over the frame.
    let idle = stripped(&render_frame(24, 100, &state));
    assert!(idle.contains("Run a command:"));
    assert!(idle.contains("Open a shell"));

    state.step_loading("エージェント起動中…");
    let pane = right_pane_contents(&state, 60, 12);
    assert!(
        pane.iter()
            .all(|line| console::strip_ansi_codes(line).trim().is_empty()),
        "loading should be a dedicated right-pane surface, not the focus menu"
    );

    let joined = stripped(&render_frame(24, 100, &state));
    assert!(joined.contains("(｡･-･)"));
    assert!(!joined.contains("Run a command:"));
    assert!(!joined.contains("Open a shell"));
}

#[test]
fn run2_loading_centers_in_the_right_pane_while_an_action_runs() {
    let mut state = state_with(Vec::new());
    state.step_loading("エージェント起動中…");
    let frame = render_frame(24, 100, &state);
    let plain: Vec<String> = frame
        .iter()
        .map(|line| console::strip_ansi_codes(line).into_owned())
        .collect();
    assert!(plain.join("\n").contains("エージェント起動中…"));
    let row = plain
        .iter()
        .position(|line| line.contains("(｡･-･)"))
        .expect("run 2 loading rabbits are rendered");
    let col = console::measure_text_width(
        plain[row]
            .split("(｡･-･)")
            .next()
            .expect("split always yields a prefix"),
    );

    let (_, width) = widgets::normalize_size(24, 100);
    let (left_w, right_w) = layout(width, state.sidebar());
    let body_start = CHROME_TOP_ROWS;
    let body_rows = body_rows_for(24);
    let expected_row = body_start + body_rows.saturating_sub(3) / 2 + 1;
    let pane_start = left_w + SEP_WIDTH;
    let pane_mid = pane_start + right_w / 2;

    assert_eq!(row, expected_row);
    assert!(col >= pane_start);
    // The first rabbit starts left of the pane midpoint because the multiplying
    // row spans several rabbits, proving the whole `usagi run 2` block is
    // centred in the right pane rather than anchored to the far right edge.
    assert!(col < pane_mid);
}

#[test]
fn run2_loading_grows_rightward_without_shifting_or_looping() {
    let first = launch_loading_block!(0, 100);
    let grown = launch_loading_block!(RUN2_LOADING_GROW * 3, 100);
    let later = launch_loading_block!(RUN2_LOADING_GROW * 99, 100);
    let plain_first = console::strip_ansi_codes(&first.join("\n")).into_owned();
    let plain_grown = console::strip_ansi_codes(&grown.join("\n")).into_owned();
    let plain_later = console::strip_ansi_codes(&later.join("\n")).into_owned();

    for (before, after) in plain_first.lines().zip(plain_grown.lines()) {
        assert!(
            after.starts_with(before.trim_end()),
            "existing rabbits stay anchored while new ones appear to the right"
        );
    }
    assert!(
        plain_grown.matches("(｡･-･)").count() > plain_first.matches("(｡･-･)").count(),
        "the warren grows"
    );
    assert_eq!(
        plain_later.matches("(｡･-･)").count(),
        RUN2_LOADING_MAX_RABBITS,
        "growth saturates at the maximum instead of looping back smaller"
    );
    assert!(
        first
            .iter()
            .zip(grown.iter())
            .all(|(a, b)| console::measure_text_width(a) == console::measure_text_width(b)),
        "every frame reserves the same block width so the centred overlay is stable"
    );
}

#[test]
fn run2_loading_has_priority_in_the_sidebar_bubble() {
    // The run 2 loading block owns the right pane during a blocking action; the
    // sidebar mascot explains that action before showing informational update
    // news.
    let mut state = state_with(Vec::new());
    state.set_update(crate::domain::version::Version::parse("9.9.9"));
    state.step_loading("作成中…");
    let joined = stripped(&render_frame(24, 100, &state));
    assert!(joined.contains("(｡･-･)"));
    assert!(joined.contains("作成中…"));
    assert!(!joined.contains("アップデートがあるぴょん"));
}

#[test]
fn run2_loading_is_skipped_when_the_terminal_is_too_narrow() {
    // Like the update notice, the block is dropped rather than clobbering the
    // chrome when it cannot fit the width.
    let mut state = state_with(Vec::new());
    state.step_loading("作成中…");
    let joined = stripped(&render_frame(24, 10, &state));
    assert!(!joined.contains("(｡･-･)"));
}

#[test]
fn env_resolve_loading_floats_over_the_right_pane_with_a_caption_below() {
    // Resolving a pane's 1Password env happens *within* the tab, so its indicator
    // is the pane-launch rabbits floated in the right pane — with the `環境変数を
    // 解決中…` caption on its own row below them, not a full-screen splash.
    let state = state_with(Vec::new());
    let frame = env_resolve_loading_frame(24, 100, &state, 0, "環境変数を解決中…");
    let plain: Vec<String> = frame
        .iter()
        .map(|line| console::strip_ansi_codes(line).into_owned())
        .collect();
    let joined = plain.join("\n");
    assert!(
        joined.contains("(｡･-･)"),
        "the launch rabbits are floated in"
    );
    assert!(joined.contains("環境変数を解決中…"), "the caption is shown");

    let rabbit_row = plain
        .iter()
        .position(|line| line.contains("(｡･-･)"))
        .expect("rabbits are rendered");
    let caption_row = plain
        .iter()
        .position(|line| line.contains("環境変数を解決中…"))
        .expect("the caption is rendered");
    assert!(
        caption_row > rabbit_row,
        "the caption sits below the rabbits"
    );

    // Centred in the right pane: the block starts past the divider and its first
    // rabbit lands left of the pane midpoint (the multiplying row spans several).
    let (_, width) = widgets::normalize_size(24, 100);
    let (left_w, right_w) = layout(width, state.sidebar());
    let pane_start = left_w + SEP_WIDTH;
    let col = console::measure_text_width(
        plain[rabbit_row]
            .split("(｡･-･)")
            .next()
            .expect("split always yields a prefix"),
    );
    assert!(col >= pane_start);
    assert!(col < pane_start + right_w / 2);
}

#[test]
fn env_resolve_loading_is_skipped_when_the_pane_is_too_narrow() {
    // Too narrow for even one rabbit: the whole indicator is dropped rather than
    // clobbering the chrome, so the caption never shows on its own either.
    let state = state_with(Vec::new());
    let joined = stripped(&env_resolve_loading_frame(
        24,
        10,
        &state,
        0,
        "環境変数を解決中…",
    ));
    assert!(!joined.contains("(｡･-･)"));
    assert!(!joined.contains("環境変数を解決中…"));
}

#[test]
fn env_resolve_loading_clips_a_long_caption_to_the_pane() {
    // A caption wider than the pane is clipped with an ellipsis instead of
    // widening the block past the pane (which would drop the whole indicator).
    let state = state_with(Vec::new());
    let long = "とても長い環境変数".repeat(20);
    let frame = env_resolve_loading_frame(24, 100, &state, 0, &long);
    let plain: Vec<String> = frame
        .iter()
        .map(|line| console::strip_ansi_codes(line).into_owned())
        .collect();

    // The rabbits survive: the block was not dropped, which proves the (now
    // clipped) caption did not widen it past the pane — an over-wide block is
    // skipped whole, rabbits and all.
    let caption_row = plain
        .iter()
        .position(|line| line.contains("とても長い環境変数"))
        .expect("the clipped caption is rendered");
    assert!(plain.join("\n").contains("(｡･-･)"), "the rabbits survive");
    assert!(plain[caption_row].contains('…'), "the caption is clipped");
}

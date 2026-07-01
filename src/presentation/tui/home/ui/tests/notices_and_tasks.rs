use super::*;

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
    // The sidebar still shows the per-session `◆ waiting` row; the header adds
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
fn render_frame_hides_waiting_notice_while_task_status_has_the_corner() {
    use super::super::super::tasks::{TaskMark, TaskRow};
    let mut waiting = worktree(Some("fix"), false, BranchStatus::Pushed);
    waiting.path = PathBuf::from("/repo/wait");
    let mut state = state_with(vec![waiting]);
    state.apply_badges(MonitorSnapshot {
        live: [PathBuf::from("/repo/wait")].into(),
        waiting: [PathBuf::from("/repo/wait")].into(),
        ..Default::default()
    });
    state.set_tasks(vec![TaskRow {
        label: "作成中… main".to_string(),
        mark: TaskMark::Running(0),
    }]);
    let frame = render_frame(24, 100, &state);
    let header = stripped(&[frame[0].clone()]);
    assert!(header.contains("作成中… main"));
    assert!(!header.contains(" 1 waiting"));
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
            label: "削除完了 feat".to_string(),
            mark: TaskMark::Done(true),
        },
        TaskRow {
            label: "作成中… main".to_string(),
            mark: TaskMark::Running(0),
        },
        TaskRow {
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
            label: "作成完了 a".to_string(),
            mark: TaskMark::Done(true),
        },
        TaskRow {
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
            label: "作成完了 a".to_string(),
            mark: TaskMark::Done(true),
        },
        TaskRow {
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
            label: "作成中… a".to_string(),
            mark: TaskMark::Running(0),
        }],
        100,
    );
    let long = task_status_line(
        &[TaskRow {
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
fn render_frame_shows_the_task_status_alongside_the_spoken_update() {
    use super::super::super::tasks::{TaskMark, TaskRow};
    let mut state = state_with(Vec::new());
    // The task status (top-right corner) and the update notice (sidebar mascot)
    // now live in different places, so both show at once.
    state.set_update(crate::domain::version::Version::parse("9.9.9"));
    state.set_tasks(vec![TaskRow {
        label: "作成中… main".to_string(),
        mark: TaskMark::Running(0),
    }]);
    let joined = stripped(&render_frame(24, 100, &state));
    assert!(joined.contains("作成中… main"));
    assert!(joined.contains("アップデートがあるぴょん"));
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
        label: "作成中… main".to_string(),
        mark: TaskMark::Running(0),
    }]);
    let frame = render_frame(24, 100, &state);
    let joined = stripped(&frame);
    // The status shows on the header row and the shell output stays intact.
    assert!(joined.contains("作成中… main"));
    assert!(joined.contains("$ echo hi"));
    // The status rides row 0 (the title bar), not the body where the shell is.
    assert!(stripped(&[frame[0].clone()]).contains("作成中… main"));
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
fn render_frame_keeps_the_loading_rabbit_over_a_live_terminal() {
    // The transient launch indicator is deliberate, so it still takes the corner
    // even while a live preview is on screen (it is painted during the blocking
    // terminal / agent spawn, before the new pane draws over the screen).
    let mut state = state_with(vec![worktree(Some("main"), true, BranchStatus::Pushed)]);
    state.enter_focus(1);
    state.show_attached();
    state.set_terminal_view(TerminalView::from_rows(vec!["$ ".to_string()], None));
    state.step_loading("ターミナル起動中…");
    let joined = stripped(&render_frame(24, 100, &state));
    assert!(joined.contains("ターミナル起動中…"));
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
fn render_frame_shows_the_loading_rabbit_while_an_action_runs() {
    let mut state = state_with(Vec::new());
    state.step_loading("削除中… 1/2");
    let joined = stripped(&render_frame(24, 100, &state));
    assert!(joined.contains("削除中… 1/2"));
    // The hopping rabbit's face rides the corner.
    assert!(joined.contains("(･ㅅ･)"));
}

#[test]
fn loading_rabbit_takes_the_corner_while_the_sidebar_speaks_the_update() {
    // The loading rabbit owns the top-right corner during a blocking action; the
    // update notice lives on the sidebar mascot, so both show at once.
    let mut state = state_with(Vec::new());
    state.set_update(crate::domain::version::Version::parse("9.9.9"));
    state.step_loading("作成中…");
    let joined = stripped(&render_frame(24, 100, &state));
    assert!(joined.contains("作成中…"));
    assert!(joined.contains("アップデートがあるぴょん"));
}

#[test]
fn loading_rabbit_is_skipped_when_the_terminal_is_too_narrow() {
    // Like the update notice, the block is dropped rather than clobbering the
    // chrome when it cannot fit the width.
    let mut state = state_with(Vec::new());
    state.step_loading("作成中…");
    let joined = stripped(&render_frame(24, 10, &state));
    assert!(!joined.contains("作成中…"));
}

//! File finder and read-only text viewer for the Preview overlay.

use unicode_segmentation::UnicodeSegmentation;

use crate::presentation::theme::{Role, Style};
use crate::presentation::views::text_overlay::{self, OverlayDocument, TextOverlay};
use crate::presentation::widgets::{self, modal};
use crate::usecase::application::controller::{
    PreviewFileFilter, PreviewOverlay, PreviewSearchMatch,
};

const INNER_WIDTH: usize = 108;
const BODY_HEIGHT: usize = 24;

/// Compose Preview over an existing Home frame.
#[must_use]
pub fn render_over(
    height: usize,
    width: usize,
    base: &[String],
    state: &PreviewOverlay,
) -> Vec<String> {
    if let Some(path) = state.path() {
        return render_document(height, width, base, state, path);
    }
    render_finder(height, width, base, state)
}

fn render_finder(
    height: usize,
    width: usize,
    base: &[String],
    state: &PreviewOverlay,
) -> Vec<String> {
    let inner = modal::modal_inner_width(width, INNER_WIDTH);
    let body_height = modal::reserved_body_height(height, width, BODY_HEIGHT);
    let visible = state.visible_files();
    let rows = visible
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let marker = modal::selection_marker(index == state.selected());
            let row = format!("{marker} {path}");
            let row = if index == state.selected() {
                Role::Accent.style().bold().paint(&row)
            } else {
                row
            };
            modal::content_line(&row, inner)
        })
        .collect::<Vec<_>>();

    let mut body = vec![file_tabs(state.file_filter())];
    body.push(modal::filter_line(
        state.filter(),
        state.filter().len(),
        None,
    ));
    if state.is_loading() {
        body.push(modal::empty_notice("Loading files…"));
    } else if let Some(error) = state.error() {
        body.push(modal::error_line(error.message.as_str(), inner));
    } else if rows.is_empty() {
        body.push(modal::empty_notice(if state.filter().is_empty() {
            match state.file_filter() {
                PreviewFileFilter::All => "No files available.",
                PreviewFileFilter::Changed => "No changed files.",
                PreviewFileFilter::Tracked => "No tracked files.",
            }
        } else {
            "No files match the filter."
        }));
    } else {
        let file_rows = body_height.saturating_sub(3);
        body.extend(modal::bounded_list_rows(&rows, state.selected(), file_rows));
    }
    body.push(modal::footer(
        "←→ scope / type filter / ↑↓ select / Enter preview / Esc close",
    ));
    modal::render_body_over(
        height,
        width,
        base,
        "Preview files",
        inner,
        BODY_HEIGHT,
        body,
    )
}

fn file_tabs(active: PreviewFileFilter) -> String {
    let choices = PreviewFileFilter::TABS.map(|filter| {
        let role = match filter {
            PreviewFileFilter::All => Role::Accent,
            PreviewFileFilter::Changed => Role::Warning,
            PreviewFileFilter::Tracked => Role::Info,
        };
        (filter.label(), role)
    });
    modal::choice_buttons(active.tab_index(), &choices)
}

fn render_document(
    height: usize,
    width: usize,
    base: &[String],
    state: &PreviewOverlay,
    path: &str,
) -> Vec<String> {
    let inner = modal::modal_inner_width(width, INNER_WIDTH);
    let (projected_lines, projected_scroll) = project_document(state, inner);
    let document = state.error().map_or_else(
        || {
            if state.is_loading() {
                OverlayDocument::Ready(vec![Style::new().dim().paint("Loading preview…")])
            } else {
                OverlayDocument::Ready(projected_lines)
            }
        },
        |error| OverlayDocument::Unavailable(error.message.as_str().to_owned()),
    );
    let matches = state.search_matches();
    let match_position = if matches.is_empty() {
        "0/0".to_owned()
    } else {
        format!(
            "{}/{}",
            state.current_match().min(matches.len() - 1) + 1,
            matches.len()
        )
    };
    let title = if state.search().is_empty() {
        format!("Preview · {path}")
    } else {
        let query = widgets::clip_to_width(state.search(), 32);
        format!("Preview · {path} · /{query} [{match_position}]")
    };
    let footer = if state.is_search_editing() {
        let query = widgets::clip_to_width(state.search(), 64);
        format!("Enter/Esc: finish search  /{query}▌")
    } else {
        format!(
            "Esc: files  /: search  n/N: match  l: lines {}  w: wrap {}  ↑↓: scroll",
            on_off(state.show_line_numbers()),
            on_off(state.wrap_lines())
        )
    };
    text_overlay::render_over_with_layout(
        height,
        width,
        base,
        &TextOverlay::new(title, document)
            .scrolled_to(projected_scroll)
            .with_footer(footer),
        INNER_WIDTH,
        BODY_HEIGHT,
    )
}

const fn on_off(enabled: bool) -> &'static str {
    if enabled { "on" } else { "off" }
}

fn project_document(state: &PreviewOverlay, inner_width: usize) -> (Vec<String>, usize) {
    let matches = state.search_matches();
    let active_match = state.current_match().min(matches.len().saturating_sub(1));
    let number_width = state.lines().len().max(1).to_string().len();
    let number_prefix_width = if state.show_line_numbers() {
        number_width + 3
    } else {
        0
    };
    let content_width = inner_width.saturating_sub(number_prefix_width).max(1);
    let mut rows = Vec::new();
    let mut active_match_row = None;

    for (line_index, line) in state.lines().iter().enumerate() {
        let line_match_start = matches.partition_point(|matched| matched.line() < line_index);
        let line_match_end = matches.partition_point(|matched| matched.line() <= line_index);
        let line_matches = &matches[line_match_start..line_match_end];
        let segments = if state.wrap_lines() {
            wrapped_ranges(line, content_width)
        } else {
            vec![(0, line.len())]
        };
        for (segment_index, &(segment_start, segment_end)) in segments.iter().enumerate() {
            let prefix = line_number_prefix(
                line_index,
                segment_index,
                number_width,
                state.show_line_numbers(),
            );
            let highlighted = highlight_segment(
                line,
                segment_start,
                segment_end,
                line_matches,
                line_match_start,
                active_match,
            );
            if matches.get(active_match).is_some_and(|matched| {
                matched.line() == line_index
                    && matched.start() < segment_end
                    && matched.end() > segment_start
            }) {
                active_match_row.get_or_insert(rows.len());
            }
            rows.push(format!("{prefix}{highlighted}"));
        }
    }

    let scroll = state.projected_scroll(active_match_row, rows.len());
    (rows, scroll)
}

fn wrapped_ranges(line: &str, width: usize) -> Vec<(usize, usize)> {
    if line.is_empty() {
        return vec![(0, 0)];
    }
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut current_width = 0usize;
    for (offset, grapheme) in line.grapheme_indices(true) {
        let grapheme_width = widgets::display_width(grapheme);
        if current_width.saturating_add(grapheme_width) > width && offset > start {
            ranges.push((start, offset));
            start = offset;
            current_width = 0;
        }
        current_width = current_width.saturating_add(grapheme_width);
    }
    ranges.push((start, line.len()));
    ranges
}

fn line_number_prefix(
    line_index: usize,
    segment_index: usize,
    number_width: usize,
    enabled: bool,
) -> String {
    if !enabled {
        return String::new();
    }
    let number = if segment_index == 0 {
        format!("{:>number_width$}", line_index + 1)
    } else {
        " ".repeat(number_width)
    };
    Style::new().dim().paint(&format!("{number} │ "))
}

fn highlight_segment(
    line: &str,
    segment_start: usize,
    segment_end: usize,
    matches: &[PreviewSearchMatch],
    match_offset: usize,
    active_match: usize,
) -> String {
    let mut highlighted = String::new();
    let mut run_start = segment_start;
    let mut run_kind = None;
    for (relative_start, grapheme) in line[segment_start..segment_end].grapheme_indices(true) {
        let start = segment_start + relative_start;
        let end = start + grapheme.len();
        let first = matches.partition_point(|matched| matched.end() <= start);
        let after = matches.partition_point(|matched| matched.start() < end);
        let kind = if (first..after).any(|index| match_offset + index == active_match) {
            Highlight::Active
        } else if first < after {
            Highlight::Inactive
        } else {
            Highlight::Plain
        };
        if let Some(previous) = run_kind
            && previous != kind
        {
            push_highlighted(&mut highlighted, &line[run_start..start], previous);
            run_start = start;
        }
        run_kind = Some(kind);
    }
    push_highlighted(
        &mut highlighted,
        &line[run_start..segment_end],
        run_kind.unwrap_or(Highlight::Plain),
    );
    highlighted
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Highlight {
    Plain,
    Inactive,
    Active,
}

fn push_highlighted(output: &mut String, text: &str, highlight: Highlight) {
    match highlight {
        Highlight::Plain => output.push_str(text),
        Highlight::Inactive => output.push_str(&Role::Warning.style().bold().paint(text)),
        Highlight::Active => output.push_str(&Role::Accent.style().bold().reverse().paint(text)),
    }
}

#[cfg(test)]
mod tests {
    use usagi_core::domain::id::{SessionId, WorkspaceId};

    use super::*;
    use crate::presentation::widgets::{display_width, strip_ansi};
    use crate::usecase::application::controller::{
        AppEvent, AppKey, AppState, BackendEvent, SafeError, SafeMessage, Target, update,
    };

    fn joined(state: &AppState) -> String {
        let base = vec!["background".to_owned(); 24];
        render_over(24, 90, &base, state.preview_overlay().unwrap())
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn complete_preview(
        state: &mut AppState,
        target: Target,
        path: Option<&str>,
        filter: PreviewFileFilter,
        files: Vec<String>,
        lines: Vec<String>,
    ) {
        let request_id = state.preview_overlay().unwrap().request_id();
        let _ = update(
            state,
            AppEvent::Backend(BackendEvent::PreviewLoaded {
                target,
                request_id,
                path: path.map(str::to_owned),
                filter,
                files,
                lines,
            }),
        );
    }

    fn fail_preview(state: &mut AppState, target: Target, message: &str) {
        let overlay = state.preview_overlay().unwrap();
        let request_id = overlay.request_id();
        let filter = overlay.file_filter();
        let _ = update(
            state,
            AppEvent::Backend(BackendEvent::PreviewError {
                target,
                request_id,
                path: None,
                filter,
                error: SafeError {
                    message: SafeMessage::new(message),
                    error_id: "preview-files".into(),
                },
            }),
        );
    }

    #[test]
    fn finder_and_document_render_their_controls_and_states() {
        let workspace = WorkspaceId::new();
        let target = Target::Session(SessionId::new());
        let mut state = AppState::home(workspace, vec![target.session_id().unwrap()]);
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenPreview));
        assert!(joined(&state).contains("Loading files"));

        complete_preview(
            &mut state,
            target,
            None,
            PreviewFileFilter::All,
            vec!["README.md".into(), "src/main.rs".into()],
            vec![],
        );
        let finder = joined(&state);
        assert!(finder.contains("Preview files"));
        assert!(finder.contains("README.md"));
        assert!(finder.contains("all"));
        assert!(finder.contains("changed"));
        assert!(finder.contains("tracked"));
        assert!(finder.contains("type filter"));

        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        assert!(joined(&state).contains("Loading preview"));
        complete_preview(
            &mut state,
            target,
            Some("README.md"),
            PreviewFileFilter::All,
            vec![],
            vec!["hello".into()],
        );
        let document = joined(&state);
        assert!(document.contains("Preview · README.md"));
        assert!(document.contains("hello"));
        assert!(document.contains("Esc: files"));
    }

    #[test]
    fn finder_empty_filter_and_error_states_stay_inside_the_frame() {
        let workspace = WorkspaceId::new();
        let target = Target::Session(SessionId::new());
        let mut state = AppState::home(workspace, vec![target.session_id().unwrap()]);
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenPreview));
        complete_preview(
            &mut state,
            target,
            None,
            PreviewFileFilter::All,
            vec![],
            vec![],
        );
        assert!(joined(&state).contains("No files available"));
        let _ = update(&mut state, AppEvent::Key(AppKey::Char('x')));
        assert!(joined(&state).contains("No files match"));

        fail_preview(&mut state, target, "files unavailable");
        let frame = render_over(
            9,
            30,
            &vec!["background".into(); 9],
            state.preview_overlay().unwrap(),
        );
        assert!(frame.iter().all(|line| display_width(line) <= 30));
    }

    #[test]
    fn finder_and_document_use_the_large_preview_layout() {
        let workspace = WorkspaceId::new();
        let target = Target::Session(SessionId::new());
        let mut state = AppState::home(workspace, vec![target.session_id().unwrap()]);
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenPreview));
        complete_preview(
            &mut state,
            target,
            None,
            PreviewFileFilter::All,
            (0..30)
                .map(|index| format!("src/file-{index}.rs"))
                .collect(),
            vec![],
        );

        let base = vec![String::new(); 40];
        let finder = render_over(40, 120, &base, state.preview_overlay().unwrap());
        let finder_box_rows = finder
            .iter()
            .map(|line| strip_ansi(line))
            .filter(|line| line.contains('┌') || line.contains('│') || line.contains('└'))
            .count();
        assert_eq!(finder_box_rows, BODY_HEIGHT + 4);
        let finder_top = finder
            .iter()
            .map(|line| strip_ansi(line))
            .find(|line| line.contains("Preview files"))
            .unwrap();
        assert_eq!(display_width(finder_top.trim()), INNER_WIDTH + 4);

        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        complete_preview(
            &mut state,
            target,
            Some("src/file-0.rs"),
            PreviewFileFilter::All,
            vec![],
            vec!["body".into()],
        );
        let document = render_over(40, 120, &base, state.preview_overlay().unwrap());
        let document_box_rows = document
            .iter()
            .map(|line| strip_ansi(line))
            .filter(|line| line.contains('┌') || line.contains('│') || line.contains('└'))
            .count();
        assert_eq!(document_box_rows, BODY_HEIGHT + 4);
        let document_top = document
            .iter()
            .map(|line| strip_ansi(line))
            .find(|line| line.contains("Preview · src/file-0.rs"))
            .unwrap();
        assert_eq!(display_width(document_top.trim()), INNER_WIDTH + 4);
    }

    #[test]
    fn short_scrolled_document_keeps_the_back_control_inside_the_frame() {
        let workspace = WorkspaceId::new();
        let target = Target::Session(SessionId::new());
        let mut state = AppState::home(workspace, vec![target.session_id().unwrap()]);
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenPreview));
        complete_preview(
            &mut state,
            target,
            None,
            PreviewFileFilter::All,
            vec!["src/lib.rs".into()],
            vec![],
        );
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        complete_preview(
            &mut state,
            target,
            Some("src/lib.rs"),
            PreviewFileFilter::All,
            vec![],
            vec!["first".into(), "selected".into(), "last".into()],
        );
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));

        let frame = render_over(
            10,
            40,
            &vec!["background".into(); 10],
            state.preview_overlay().unwrap(),
        );
        let plain = frame
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(plain.contains("selected"));
        assert!(plain.contains("Esc: files"));
        assert!(frame.iter().all(|line| display_width(line) == 40));
        assert!(frame.first().unwrap().starts_with("background"));
        assert!(frame.last().unwrap().starts_with("background"));
    }

    #[test]
    fn document_search_highlights_matches_and_projects_line_numbers_and_wraps() {
        let workspace = WorkspaceId::new();
        let target = Target::Session(SessionId::new());
        let mut state = AppState::home(workspace, vec![target.session_id().unwrap()]);
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenPreview));
        complete_preview(
            &mut state,
            target,
            None,
            PreviewFileFilter::All,
            vec!["src/lib.rs".into()],
            vec![],
        );
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        complete_preview(
            &mut state,
            target,
            Some("src/lib.rs"),
            PreviewFileFilter::All,
            vec![],
            vec![
                "needle crosses a deliberately long source line with needle".into(),
                String::new(),
            ],
        );
        let _ = update(&mut state, AppEvent::Key(AppKey::Char('/')));
        let _ = update(&mut state, AppEvent::Key(AppKey::Paste("needle".into())));
        assert!(joined(&state).contains("/needle▌"));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let _ = update(&mut state, AppEvent::Key(AppKey::Char('l')));
        let _ = update(&mut state, AppEvent::Key(AppKey::Char('w')));

        let overlay = state.preview_overlay().unwrap();
        let (rows, scroll) = project_document(overlay, 24);
        let plain = rows.iter().map(|row| strip_ansi(row)).collect::<Vec<_>>();
        assert!(rows.len() > overlay.lines().len());
        assert!(plain[0].starts_with("1 │ needle"));
        assert!(plain.iter().any(|row| row.starts_with("  │ ")));
        assert!(plain.last().unwrap().starts_with("2 │ "));
        assert!(rows.join("\n").contains("\u{1b}[1;7;36mneedle"));
        assert!(rows.join("\n").contains("\u{1b}[1;33mneedle"));
        assert_eq!(scroll, 0);

        let frame = joined(&state);
        assert!(frame.contains("/needle [1/2]"));
        assert!(frame.contains("lines on"));
        assert!(frame.contains("wrap on"));
        assert!(frame.contains("n/N: match"));

        let _ = update(&mut state, AppEvent::Key(AppKey::Char('n')));
        let (_, second_scroll) = project_document(state.preview_overlay().unwrap(), 24);
        assert!(second_scroll > scroll);
        assert!(joined(&state).contains("/needle [2/2]"));
    }

    #[test]
    fn a_search_match_crossing_a_wrap_boundary_highlights_both_fragments() {
        let workspace = WorkspaceId::new();
        let target = Target::Session(SessionId::new());
        let mut state = AppState::home(workspace, vec![target.session_id().unwrap()]);
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenPreview));
        complete_preview(
            &mut state,
            target,
            None,
            PreviewFileFilter::All,
            vec!["tiny".into()],
            vec![],
        );
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        complete_preview(
            &mut state,
            target,
            Some("tiny"),
            PreviewFileFilter::All,
            vec![],
            vec!["abcd".into()],
        );
        let _ = update(&mut state, AppEvent::Key(AppKey::Char('/')));
        let _ = update(&mut state, AppEvent::Key(AppKey::Paste("bc".into())));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let _ = update(&mut state, AppEvent::Key(AppKey::Char('w')));

        let (rows, scroll) = project_document(state.preview_overlay().unwrap(), 2);
        assert_eq!(scroll, 0);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].contains("\u{1b}[1;7;36mb"));
        assert!(rows[1].contains("\u{1b}[1;7;36mc"));
        assert_eq!(strip_ansi(&rows.join("")), "abcd");
    }

    #[test]
    fn arrow_scroll_reaches_continuations_of_one_wrapped_source_line() {
        let workspace = WorkspaceId::new();
        let target = Target::Session(SessionId::new());
        let mut state = AppState::home(workspace, vec![target.session_id().unwrap()]);
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenPreview));
        complete_preview(
            &mut state,
            target,
            None,
            PreviewFileFilter::All,
            vec!["tiny".into()],
            vec![],
        );
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        complete_preview(
            &mut state,
            target,
            Some("tiny"),
            PreviewFileFilter::All,
            vec![],
            vec!["abcdef".into()],
        );
        let _ = update(&mut state, AppEvent::Key(AppKey::Char('w')));

        let (rows, initial) = project_document(state.preview_overlay().unwrap(), 2);
        assert_eq!(
            rows.iter().map(|row| strip_ansi(row)).collect::<Vec<_>>(),
            ["ab", "cd", "ef"]
        );
        assert_eq!(initial, 0);
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        assert_eq!(project_document(state.preview_overlay().unwrap(), 2).1, 1);
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        assert_eq!(project_document(state.preview_overlay().unwrap(), 2).1, 2);
    }

    #[test]
    fn wrapping_and_partial_match_highlighting_preserve_grapheme_clusters() {
        let workspace = WorkspaceId::new();
        let target = Target::Session(SessionId::new());
        let mut state = AppState::home(workspace, vec![target.session_id().unwrap()]);
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenPreview));
        complete_preview(
            &mut state,
            target,
            None,
            PreviewFileFilter::All,
            vec!["emoji".into()],
            vec![],
        );
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        complete_preview(
            &mut state,
            target,
            Some("emoji"),
            PreviewFileFilter::All,
            vec![],
            vec!["👩‍💻x".into()],
        );
        let _ = update(&mut state, AppEvent::Key(AppKey::Char('/')));
        let _ = update(&mut state, AppEvent::Key(AppKey::Paste("‍".into())));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let _ = update(&mut state, AppEvent::Key(AppKey::Char('w')));

        let (rows, scroll) = project_document(state.preview_overlay().unwrap(), 2);
        assert_eq!(scroll, 0);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].contains("\u{1b}[1;7;36m👩‍💻\u{1b}[0m"));
        assert_eq!(strip_ansi(&rows.join("")), "👩‍💻x");
    }

    #[test]
    fn finder_empty_copy_names_the_selected_file_group() {
        let workspace = WorkspaceId::new();
        let target = Target::Session(SessionId::new());
        let mut state = AppState::home(workspace, vec![target.session_id().unwrap()]);
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenPreview));
        let _ = update(&mut state, AppEvent::Key(AppKey::Right));
        complete_preview(
            &mut state,
            target,
            None,
            PreviewFileFilter::Changed,
            vec![],
            vec![],
        );
        assert!(joined(&state).contains("No changed files"));

        let _ = update(&mut state, AppEvent::Key(AppKey::Right));
        complete_preview(
            &mut state,
            target,
            None,
            PreviewFileFilter::Tracked,
            vec![],
            vec![],
        );
        assert!(joined(&state).contains("No tracked files"));
    }
}

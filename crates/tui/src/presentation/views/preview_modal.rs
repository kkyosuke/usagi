//! File finder and read-only text viewer for the Preview overlay.

use unicode_segmentation::UnicodeSegmentation;

use crate::presentation::theme::{Role, Style};
use crate::presentation::views::text_overlay::{self, OverlayDocument, TextOverlay};
use crate::presentation::widgets::{self, modal};
use crate::usecase::application::controller::{
    PreviewCandidate, PreviewFileFilter, PreviewOverlay, PreviewSearchMatch,
};

/// Preview だけは 1 ファイルの本文を読むための overlay なので、他の modal の
/// ように固定寸法にせず端末いっぱいに近い枠を取る。枠の外に残す背景は左右
/// [`HORIZONTAL_MARGIN`] 桁・上下 [`VERTICAL_MARGIN`] 行だけで、残りはすべて
/// 本文に充てる。狭い端末では下限（`MIN_*`）を希望値に据えるため、`modal` 側の
/// clip が従来どおり「枠いっぱい」に収める（この背景は下限を超える端末でだけ
/// ちょうど残り、下限付近では clip に食われる）。希望寸法は正規化前の生の端末
/// サイズから決めるが、`0` は下限へ落ちるので `modal` 側の
/// [`normalize_size`](crate::presentation::widgets::normalize_size) と食い違わない。
const MIN_INNER_WIDTH: usize = 108;
const MIN_BODY_HEIGHT: usize = 24;
const HORIZONTAL_MARGIN: usize = 3;
const VERTICAL_MARGIN: usize = 2;

/// 端末幅に追随する希望内側幅。box 幅は内側幅 + 4（枠と左右 padding）なので、
/// 左右の背景を差し引いた残りを返す。実際の clip は
/// [`modal::modal_inner_width`] が行う。
fn desired_inner_width(width: usize) -> usize {
    width
        .saturating_sub(4 + HORIZONTAL_MARGIN * 2)
        .max(MIN_INNER_WIDTH)
}

/// 端末高に追随する希望本文行数。box 高は本文 + 4（枠と上下 padding）なので、
/// 上下の背景を差し引いた残りを返す。実際の clip は
/// [`modal::reserved_body_height`] が行う。
fn desired_body_height(height: usize) -> usize {
    height
        .saturating_sub(4 + VERTICAL_MARGIN * 2)
        .max(MIN_BODY_HEIGHT)
}

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
    let inner = modal::modal_inner_width(width, desired_inner_width(width));
    let desired_body = desired_body_height(height);
    let body_height = modal::reserved_body_height(height, width, desired_body);
    let candidates = state.visible_candidates();
    let name_column = name_column_width(&candidates, inner);
    let rows = candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            candidate_row(candidate, index == state.selected(), name_column, inner)
        })
        .collect::<Vec<_>>();

    let mut body = vec![file_tabs(state.file_filter())];
    body.push(count_line(state, candidates.len(), inner));
    if state.is_loading() {
        body.push(modal::empty_notice("Loading files…"));
    } else if let Some(error) = state.error() {
        body.push(modal::error_line(error.message.as_str(), inner));
    } else if rows.is_empty() {
        body.extend(empty_rows(state, inner));
    } else {
        let file_rows = body_height.saturating_sub(3);
        let (rows, selected) = match state.rest_sections() {
            Some(sections) => sectioned_rows(rows, sections, state.selected()),
            None => (rows, state.selected()),
        };
        body.extend(modal::bounded_list_rows(&rows, selected, file_rows));
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
        desired_body,
        body,
    )
}

/// Insert the resting view's section headings and move the cursor with them.
///
/// The resting rows are two lists in one column — recently opened files, then
/// changed ones — so the headings have to be part of the same scrolling body
/// the cursor lives in. The returned index points at the same candidate after
/// the headings are interleaved.
fn sectioned_rows(
    rows: Vec<String>,
    (recent, changed): (usize, usize),
    selected: usize,
) -> (Vec<String>, usize) {
    let mut sectioned = Vec::with_capacity(rows.len() + 2);
    let mut rows = rows.into_iter();
    if recent > 0 {
        sectioned.push(modal::caption("recent"));
        sectioned.extend(rows.by_ref().take(recent));
    }
    if changed > 0 {
        sectioned.push(modal::caption(&format!("changed · {changed}")));
        sectioned.extend(rows);
    }
    let headings = usize::from(recent > 0) + usize::from(selected >= recent && changed > 0);
    (sectioned, selected + headings)
}

/// Width of the file-name column: the longest name on screen, within the half
/// of the frame the directory column does not need.
///
/// A fixed cap would clip exactly the long names that need reading — this
/// repository has file names past 90 cells — while the frame now grows with the
/// terminal. Sizing from the rows themselves keeps short listings tight and
/// long ones readable.
fn name_column_width(candidates: &[PreviewCandidate<'_>], inner: usize) -> usize {
    let budget = inner.saturating_sub(modal::BODY_INDENT_WIDTH + 2) / 2;
    let longest = candidates
        .iter()
        .map(|candidate| widgets::display_width(file_name(candidate.path())))
        .max()
        .unwrap_or(0);
    longest.clamp(1, budget.max(1))
}

/// The file-name part of a repository-relative path.
fn file_name(path: &str) -> &str {
    &path[path.rfind('/').map_or(0, |index| index + 1)..]
}

/// One finder row: the file name first, then its directory in a dim second
/// column, with the filter's matched cells reversed in both.
///
/// Reading a picker is reading file names; the directory only disambiguates
/// two files that share one. Splitting them keeps the names left-aligned in one
/// column instead of ending wherever their path happens to end.
fn candidate_row(
    candidate: &PreviewCandidate<'_>,
    selected: bool,
    name_column: usize,
    inner: usize,
) -> String {
    let path = candidate.path();
    let split = path.rfind('/').map_or(0, |index| index + 1);
    let directory_cells = path[..split].chars().count();
    let name_style = if selected {
        Role::Accent.style().bold()
    } else {
        Style::new()
    };
    let name = highlighted(
        &path[split..],
        candidate.positions(),
        directory_cells,
        name_style,
    );
    let marker = modal::selection_marker(selected);
    let row = if split == 0 {
        format!("{marker} {name}")
    } else {
        let directory = highlighted(
            path[..split].trim_end_matches('/'),
            candidate.positions(),
            0,
            Style::new().dim(),
        );
        format!(
            "{marker} {}  {directory}",
            widgets::pad_to_width(&name, name_column)
        )
    };
    modal::content_line(&row, inner)
}

/// Paint `text` with `base`, reversing the cells the filter matched.
///
/// `offset` is the `char` index of `text` inside the candidate path, so a slice
/// of the path can be highlighted with the positions of the whole.
fn highlighted(text: &str, positions: &[usize], offset: usize, base: Style) -> String {
    let mut out = String::new();
    let mut run = String::new();
    let mut run_matched = false;
    for (index, character) in text.chars().enumerate() {
        let matched = positions.binary_search(&(offset + index)).is_ok();
        if matched != run_matched && !run.is_empty() {
            out.push_str(&paint_run(&run, run_matched, base));
            run.clear();
        }
        run_matched = matched;
        run.push(character);
    }
    if !run.is_empty() {
        out.push_str(&paint_run(&run, run_matched, base));
    }
    out
}

fn paint_run(text: &str, matched: bool, base: Style) -> String {
    if matched {
        Role::Accent.style().bold().reverse().paint(text)
    } else {
        base.paint(text)
    }
}

/// The filter row, with the match count against the loaded group on its right.
///
/// The count is what tells a reader the filter is working: `12/3184` narrows,
/// `0/3184` says the query is wrong rather than the group being empty. While the
/// group is still loading there is no honest count to show, so the row carries
/// the filter alone.
fn count_line(state: &PreviewOverlay, matched: usize, inner: usize) -> String {
    let filter = state.filter();
    let left = modal::filter_line(filter, filter.len(), None);
    if state.is_loading() {
        return widgets::clip_to_width(&left, inner);
    }
    let total = state.total_files();
    let count = if filter.is_empty() {
        format!("{total} files")
    } else {
        format!("{matched}/{total}")
    };
    let count = Style::new().dim().paint(&count);
    let used = widgets::display_width(&left) + widgets::display_width(&count);
    if used + modal::BODY_INDENT_WIDTH > inner {
        return widgets::clip_to_width(&left, inner);
    }
    let gap = inner.saturating_sub(used + modal::BODY_INDENT_WIDTH);
    format!("{left}{}{count}", " ".repeat(gap))
}

/// Rows shown when the loaded group has no row to offer.
///
/// An empty group and an unmatched filter are different problems, so they get
/// different copy: the first names the group, the second quotes the query, says
/// how large the searched group is, and names the way out.
fn empty_rows(state: &PreviewOverlay, inner: usize) -> Vec<String> {
    if state.filter().is_empty() {
        if state.is_resting() && state.total_files() > 0 {
            return vec![modal::empty_notice(&format!(
                "Nothing opened or changed yet. Type to search {} files.",
                state.total_files()
            ))];
        }
        return vec![modal::empty_notice(match state.file_filter() {
            PreviewFileFilter::All => "No files available.",
            PreviewFileFilter::Changed => "No changed files.",
            PreviewFileFilter::Tracked => "No tracked files.",
        })];
    }
    let query = widgets::clip_to_width(state.filter(), 32);
    vec![
        modal::content_line(
            &Style::new().dim().paint(&format!(
                "No file matches “{query}” in {} ({} files).",
                state.file_filter().label(),
                state.total_files()
            )),
            inner,
        ),
        modal::content_line(
            &Style::new()
                .dim()
                .paint("←→ switches the file group; Backspace edits the filter."),
            inner,
        ),
    ]
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
    let inner = modal::modal_inner_width(width, desired_inner_width(width));
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
        search_footer(state.search(), inner)
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
        desired_inner_width(width),
        desired_body_height(height),
    )
}

fn search_footer(search: &str, inner_width: usize) -> String {
    let query_width = inner_width.saturating_sub(modal::BODY_INDENT_WIDTH + 2);
    let query = widgets::clip_to_width(search, query_width);
    format!("/{query}▌  Enter/Esc: finish search")
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
                changed: files.clone(),
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
        let unmatched = joined(&state);
        assert!(unmatched.contains("No file matches “x” in all (0 files)."));
        assert!(unmatched.contains("←→ switches the file group"));

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
    fn finder_rows_split_the_name_from_its_directory_and_mark_the_match() {
        let workspace = WorkspaceId::new();
        let target = Target::Session(SessionId::new());
        let mut state = AppState::home(workspace, vec![target.session_id().unwrap()]);
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenPreview));
        complete_preview(
            &mut state,
            target,
            None,
            PreviewFileFilter::All,
            vec![
                "crates/tui/src/presentation/views/preview_modal.rs".into(),
                "document/03-tui.md".into(),
                "Cargo.toml".into(),
            ],
            vec![],
        );

        // 空フィルタでは群の総数を出す。
        assert!(joined(&state).contains("3 files"));

        let base = vec![String::new(); 24];
        let overlay = state.preview_overlay().unwrap();
        let styled = render_over(24, 90, &base, overlay).join("\n");
        let plain = strip_ansi(&styled);
        // 名前が先頭、ディレクトリは第 2 列。列幅は画面上でいちばん長い名前に合う。
        assert!(plain.contains("preview_modal.rs  crates/tui/src/presentation/views"));
        assert!(plain.contains("03-tui.md         document"));
        // ディレクトリを持たない候補は名前だけの行になる。
        assert!(plain.contains("  Cargo.toml"));

        let _ = update(&mut state, AppEvent::Key(AppKey::Paste("prevmod".into())));
        let filtered = joined(&state);
        assert!(filtered.contains("1/3"));
        assert!(filtered.contains("preview_modal.rs"));
        assert!(!filtered.contains("03-tui.md"));

        // 一致した cell だけが反転する。ランキングが名前側を選ぶので、`prev` と
        // `mod` の 2 つの run がファイル名の中で反転する。
        let styled = render_over(24, 90, &base, state.preview_overlay().unwrap()).join("\n");
        assert!(styled.contains("\u{1b}[1;7;36mprev"));
        assert!(styled.contains("\u{1b}[1;7;36mmod"));
    }

    #[test]
    fn the_name_column_follows_the_longest_name_and_counts_wide_cells() {
        let workspace = WorkspaceId::new();
        let target = Target::Session(SessionId::new());
        let mut state = AppState::home(workspace, vec![target.session_id().unwrap()]);
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenPreview));
        complete_preview(
            &mut state,
            target,
            None,
            PreviewFileFilter::All,
            vec!["document/設計メモ.md".into(), "src/a.rs".into()],
            vec![],
        );

        let base = vec![String::new(); 24];
        let plain = render_over(24, 120, &base, state.preview_overlay().unwrap())
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>();
        // 全角を 2 桁として数えるので、ディレクトリ列の開始桁がそろう。
        let column = |needle: &str| {
            let line = plain.iter().find(|line| line.contains(needle)).unwrap();
            display_width(line.split(needle).next().unwrap())
        };
        assert_eq!(column("document"), column("src"));
        // 列幅はいちばん長い名前（全角 4 文字 + `.md` で 11 桁）に合う。
        assert_eq!(column("document"), column("設計メモ.md") + 13);

        // 読み込み中は件数を出さない（総数がまだ 0 のため）。
        let _ = update(&mut state, AppEvent::Key(AppKey::Right));
        let loading = joined(&state);
        assert!(loading.contains("Loading files"));
        assert!(!loading.contains("0 files"));
    }

    #[test]
    fn a_narrow_finder_drops_the_count_instead_of_breaking_the_frame() {
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
        let _ = update(&mut state, AppEvent::Key(AppKey::Paste("lib".into())));
        let frame = render_over(
            12,
            8,
            &vec!["background".into(); 12],
            state.preview_overlay().unwrap(),
        );
        assert!(frame.iter().all(|line| display_width(line) <= 8));
        assert!(
            !frame
                .iter()
                .map(|line| strip_ansi(line))
                .any(|line| line.contains("1/1"))
        );
    }

    #[test]
    fn the_resting_finder_draws_its_sections_and_keeps_the_cursor_with_them() {
        let workspace = WorkspaceId::new();
        let target = Target::Session(SessionId::new());
        let mut state = AppState::home(workspace, vec![target.session_id().unwrap()]);
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenPreview));
        let request_id = state.preview_overlay().unwrap().request_id();
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::PreviewLoaded {
                target,
                request_id,
                path: None,
                filter: PreviewFileFilter::All,
                files: vec![
                    "Cargo.toml".into(),
                    "src/lib.rs".into(),
                    "src/main.rs".into(),
                ],
                changed: vec!["src/lib.rs".into(), "src/main.rs".into()],
                lines: Vec::new(),
            }),
        );

        let resting = joined(&state);
        assert!(resting.contains("changed · 2"));
        assert!(!resting.contains("recent"));
        assert!(resting.contains("3 files"));
        // 全件は並べない。
        assert!(!resting.contains("Cargo.toml"));

        // cursor は heading を跨いで候補の上に乗る。
        let base = vec![String::new(); 24];
        let styled = render_over(24, 90, &base, state.preview_overlay().unwrap());
        let cursor = styled
            .iter()
            .position(|line| strip_ansi(line).contains('›'))
            .unwrap();
        assert!(strip_ansi(&styled[cursor]).contains("lib.rs"));
        assert!(strip_ansi(&styled[cursor - 1]).contains("changed · 2"));

        // 1 件開くと recent 見出しが増え、changed 側からは重複が落ちる。
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
        let reopened = joined(&state);
        assert!(reopened.contains("recent"));
        assert!(reopened.contains("changed · 1"));
    }

    #[test]
    fn a_resting_finder_with_nothing_to_show_points_at_the_search() {
        let workspace = WorkspaceId::new();
        let target = Target::Session(SessionId::new());
        let mut state = AppState::home(workspace, vec![target.session_id().unwrap()]);
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenPreview));
        let request_id = state.preview_overlay().unwrap().request_id();
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::PreviewLoaded {
                target,
                request_id,
                path: None,
                filter: PreviewFileFilter::All,
                files: vec!["Cargo.toml".into(), "src/lib.rs".into()],
                changed: Vec::new(),
                lines: Vec::new(),
            }),
        );
        assert!(joined(&state).contains("Nothing opened or changed yet. Type to search 2 files."));
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
        assert_eq!(box_rows(&finder), desired_body_height(40) + 4);
        let finder_top = titled_row(&finder, "Preview files");
        assert_eq!(
            display_width(finder_top.trim()),
            desired_inner_width(120) + 4
        );

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
        assert_eq!(box_rows(&document), desired_body_height(40) + 4);
        let document_top = titled_row(&document, "Preview · src/file-0.rs");
        assert_eq!(
            display_width(document_top.trim()),
            desired_inner_width(120) + 4
        );

        // 広い端末では枠も一緒に育ち、背景は左右 3 桁・上下 2 行だけ残る。
        let wide_base = vec![String::new(); 60];
        let wide = render_over(60, 200, &wide_base, state.preview_overlay().unwrap());
        assert_eq!(box_rows(&wide), 60 - VERTICAL_MARGIN * 2);
        let wide_top = titled_row(&wide, "Preview · src/file-0.rs");
        assert_eq!(display_width(wide_top.trim()), 200 - HORIZONTAL_MARGIN * 2);
        assert!(!strip_ansi(wide.first().unwrap()).contains('┌'));

        // 下限より狭い端末では従来どおり端末いっぱいに clip される。
        let narrow_base = vec![String::new(); 24];
        let narrow = render_over(24, 80, &narrow_base, state.preview_overlay().unwrap());
        assert_eq!(box_rows(&narrow), 24 - 2);
        let narrow_top = titled_row(&narrow, "Preview · src/file-0.rs");
        assert_eq!(display_width(narrow_top.trim()), 80);
    }

    fn box_rows(frame: &[String]) -> usize {
        frame
            .iter()
            .map(|line| strip_ansi(line))
            .filter(|line| line.contains('┌') || line.contains('│') || line.contains('└'))
            .count()
    }

    fn titled_row(frame: &[String], title: &str) -> String {
        frame
            .iter()
            .map(|line| strip_ansi(line))
            .find(|line| line.contains(title))
            .unwrap()
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
        let compact = render_over(
            9,
            30,
            &vec!["background".into(); 9],
            state.preview_overlay().unwrap(),
        );
        assert!(
            compact
                .iter()
                .map(|line| strip_ansi(line))
                .any(|line| line.contains('▌'))
        );
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

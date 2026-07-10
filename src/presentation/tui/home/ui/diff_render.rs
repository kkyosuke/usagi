//! Rendering for the right-pane diff viewer.

use console::style;
use unicode_width::UnicodeWidthChar;

use super::super::state::{DiffFocus, DiffTreeRow, DiffView};
use super::markdown_render::rgb_to_ansi256;
use super::{clip_to_width, pad_to_width};
use crate::presentation::tui::diff::{DiffRow, DiffSpan, RowKind, SplitRow};
use crate::presentation::tui::markdown::Rgb;

const DIFF_ADD_BG: u8 = 22; // dark green
const DIFF_ADD_EMPH_BG: u8 = 28; // brighter green
const DIFF_DEL_BG: u8 = 52; // dark red
const DIFF_DEL_EMPH_BG: u8 = 88; // brighter red
const DIFF_NUM_FG: u8 = 244; // dim grey line numbers
const DIFF_HUNK_FG: u8 = 37; // teal hunk headers
const DIFF_ADD_FG: u8 = 34; // green +N count
const DIFF_DEL_FG: u8 = 160; // red -N count
const DIFF_DIR_FG: u8 = 39; // blue directory node

/// The explorer column's width as a share of the pane, clamped so it stays a
/// readable file list without crowding the diff: about a third of the pane,
/// floored at [`TREE_MIN`] and capped at [`TREE_MAX`], and never wider than half.
const TREE_MIN: usize = 16;
const TREE_MAX: usize = 34;

/// The explorer column width for a `pane_w`-column diff view (see [`TREE_MIN`] /
/// [`TREE_MAX`]). Returns `None` when the pane is too narrow to split usefully, so
/// the diff falls back to filling the whole pane.
fn tree_width(pane_w: usize) -> Option<usize> {
    if pane_w < TREE_MIN + 8 {
        return None;
    }
    Some((pane_w / 3).clamp(TREE_MIN, TREE_MAX).min(pane_w / 2))
}

/// Render the right-pane diff view GitHub pull-request style: a one-row header
/// (title + layout + the selected file's scroll position) over a left directory-
/// tree explorer of the changed files beside the right diff of the selected one.
/// The explorer window keeps its cursor in view and the diff window is clamped so
/// the file's last row stays visible (matching the event loop's scroll clamp);
/// every row fits the pane width so the layout never shifts.
pub(super) fn diff_pane(view: &DiffView, width: usize, rows: usize) -> Vec<String> {
    // An empty patch (a session that changed nothing) shows a friendly line
    // instead of an explorer. An empty patch has no selected file, so the same
    // check drives both the friendly line and the columns below.
    let Some(file) = view.selected_file() else {
        let mut lines = Vec::with_capacity(rows);
        lines.push(diff_header(view, width));
        if rows > 1 {
            lines.push(
                style(clip_to_width(
                    "No changes against the base branch 🐰",
                    width,
                ))
                .to_string(),
            );
            lines.resize(rows, String::new());
        }
        lines.truncate(rows);
        return lines;
    };

    let mut lines = Vec::with_capacity(rows);
    lines.push(diff_header(view, width));
    if rows <= 1 {
        lines.truncate(rows);
        return lines;
    }
    let body_h = rows - 1;
    if view.stacked() {
        lines.extend(stacked_body(view, file, width, body_h));
    } else {
        lines.extend(side_by_side_body(view, file, width, body_h));
    }
    lines
}

/// The explorer-left / diff-right body: a `tree_w`-wide explorer column beside the
/// diff, split by a vertical bar. A pane too narrow to spare the explorer shows
/// just the diff, full width. Returns exactly `body_h` rows.
fn side_by_side_body(
    view: &DiffView,
    file: &crate::presentation::tui::diff::DiffFile,
    width: usize,
    body_h: usize,
) -> Vec<String> {
    let Some(tree_w) = tree_width(width) else {
        return diff_column(view, file, width, body_h);
    };
    let diff_w = width - tree_w - 1;
    // Both columns return exactly `body_h` rows, so zipping them pairs every row.
    let tree = tree_column(view, tree_w, body_h);
    let diff = diff_column(view, file, diff_w, body_h);
    let sep = if matches!(view.focus(), DiffFocus::Tree) {
        style("│").color256(DIFF_DIR_FG).to_string()
    } else {
        style("│").dim().to_string()
    };
    tree.into_iter()
        .zip(diff)
        .map(|(left, right)| format!("{left}{sep}{right}"))
        .collect()
}

/// The explorer-on-top / diff-below body: a bounded explorer band, a full-width
/// horizontal rule, then the diff filling the rest — both full width, so this
/// stays usable even on a pane too narrow to sit them side by side. Returns
/// exactly `body_h` rows; falls back to a full-height diff when there is no room
/// to stack (`body_h < 3`).
fn stacked_body(
    view: &DiffView,
    file: &crate::presentation::tui::diff::DiffFile,
    width: usize,
    body_h: usize,
) -> Vec<String> {
    let tree_h = stacked_tree_height(view, body_h);
    if tree_h == 0 {
        return diff_column(view, file, width, body_h);
    }
    let mut out = tree_column(view, width, tree_h);
    let rule = "─".repeat(width);
    out.push(if matches!(view.focus(), DiffFocus::Tree) {
        style(rule).color256(DIFF_DIR_FG).to_string()
    } else {
        style(rule).dim().to_string()
    });
    let diff_h = body_h - tree_h - 1;
    out.extend(diff_column(view, file, width, diff_h));
    out
}

/// How many rows the explorer band gets in the stacked layout: as many as it has
/// visible rows, capped at about a third of the body (min 1) and always leaving at
/// least the rule plus one diff row. `0` when the body is too short to stack.
fn stacked_tree_height(view: &DiffView, body_h: usize) -> usize {
    if body_h < 3 {
        return 0;
    }
    let cap = (body_h / 3).clamp(1, 10);
    view.visible_rows().len().min(cap).clamp(1, body_h - 2)
}

/// The diff view's one-row header: an icon, the branch → base title, the changed-
/// file count, the layout name, and the selected file's `start-end/total` scroll
/// position once it overflows.
fn diff_header(view: &DiffView, width: usize) -> String {
    let files = view.file_count();
    if view.is_empty() {
        return style(clip_to_width(&format!(" {}", view.title), width))
            .bold()
            .to_string();
    }
    let layout = if view.split() { "split" } else { "unified" };
    let noun = if files == 1 { "file" } else { "files" };
    let header = format!(" {}  ·  {files} {noun}  [{layout}]", view.title);
    style(clip_to_width(&header, width)).bold().to_string()
}

/// Render the left explorer column: the visible tree rows windowed so the cursor
/// stays in view, each padded to exactly `width` columns so the separator and the
/// diff column line up. Always returns exactly `body_h` rows (blank padding below
/// a short tree).
fn tree_column(view: &DiffView, width: usize, body_h: usize) -> Vec<String> {
    let rows = view.visible_rows();
    let focus = view.focus();
    let cursor = rows.iter().position(|r| r.selected).unwrap_or(0);
    // Window the list so the cursor is always visible, showing as much of the tree
    // as fits below it.
    let start = if cursor >= body_h {
        cursor - body_h + 1
    } else {
        0
    }
    .min(rows.len().saturating_sub(body_h));
    let mut out: Vec<String> = rows
        .iter()
        .skip(start)
        .take(body_h)
        .map(|row| tree_cell(row, focus, width))
        .collect();
    out.resize(body_h, " ".repeat(width));
    out
}

/// Render one explorer row into exactly `width` columns: an indent for its depth,
/// a `▸`/`▾` marker for a (collapsed/expanded) directory or the file name, and the
/// file's `+A -B` counts. The cursor row is reversed (bold while the explorer has
/// the focus, plain while the diff pane does, so it still reads as "the open
/// file").
fn tree_cell(row: &DiffTreeRow, focus: DiffFocus, width: usize) -> String {
    let indent = "  ".repeat(row.depth);
    if row.selected {
        // A selection highlight overrides the per-part colours: build the plain
        // text, pad it, then reverse the whole row.
        let marker = if row.is_dir {
            if row.collapsed {
                "▸ "
            } else {
                "▾ "
            }
        } else {
            ""
        };
        let name = if row.is_dir {
            format!("{}/", row.segment)
        } else {
            row.segment.clone()
        };
        let counts = counts_text(row);
        let plain = clip_to_width(&format!("{indent}{marker}{name}{counts}"), width);
        let padded = pad_to_width(plain, width);
        let styled = style(padded).reverse();
        return if matches!(focus, DiffFocus::Tree) {
            styled.bold().to_string()
        } else {
            styled.dim().to_string()
        };
    }
    if row.is_dir {
        let marker = if row.collapsed { "▸ " } else { "▾ " };
        let text = clip_to_width(&format!("{indent}{marker}{}/", row.segment), width);
        return pad_to_width(style(text).color256(DIFF_DIR_FG).bold().to_string(), width);
    }
    // A file: the name, then its add/remove counts coloured green / red.
    let name = clip_to_width(&format!("{indent}{}", row.segment), width);
    let counts = counts_text(row);
    let styled_counts = style(&counts)
        .color256(if row.added >= row.removed {
            DIFF_ADD_FG
        } else {
            DIFF_DEL_FG
        })
        .to_string();
    pad_to_width(format!("{name}{styled_counts}"), width)
}

/// The ` +A -B` count suffix for a file row (empty for a directory or a file with
/// no counted lines, e.g. a binary change).
fn counts_text(row: &DiffTreeRow) -> String {
    if row.is_dir || (row.added == 0 && row.removed == 0) {
        String::new()
    } else {
        format!("  +{} -{}", row.added, row.removed)
    }
}

/// Render the right diff column: a `body_h`-row window of the selected `file`'s
/// section, laid out unified or side-by-side, scrolled by the view's per-file
/// offset and clamped so the file's last row stays in view.
fn diff_column(
    view: &DiffView,
    file: &crate::presentation::tui::diff::DiffFile,
    width: usize,
    body_h: usize,
) -> Vec<String> {
    let section = &view.doc.rows[file.start..file.end];
    let num_w = num_width(section);
    let split = view
        .split()
        .then(|| view.selected_split_rows().unwrap_or(&[]));
    let total = split.as_ref().map_or(section.len(), |rows| rows.len());
    let max_start = total.saturating_sub(body_h);
    let start = view.scroll().min(max_start);
    let mut out = Vec::with_capacity(body_h);
    for i in 0..body_h {
        let row = match &split {
            Some(split) => split
                .get(start + i)
                .map(|sr| diff_split_row(&view.doc, *sr, num_w, width)),
            None => view
                .doc
                .rows
                .get(file.start + start + i)
                .filter(|_| start + i < total)
                .map(|r| diff_unified_row(r, num_w, width)),
        };
        out.push(row.unwrap_or_default());
    }
    out
}

/// The line-number gutter width for a file section: the digit count of its largest
/// line number (at least 2), so the gutter is as narrow as the content allows.
fn num_width(rows: &[DiffRow]) -> usize {
    let max = rows
        .iter()
        .filter_map(|r| r.old_no.max(r.new_no))
        .max()
        .unwrap_or(0);
    (max.to_string().len()).max(2)
}

/// Render one diff row in the unified layout: a `old new` line-number gutter, a
/// `+`/`-`/space marker, then the syntax-highlighted content on its add/del/
/// context background. Header / hunk / meta rows span the full width instead.
fn diff_unified_row(row: &DiffRow, num_w: usize, width: usize) -> String {
    match row.kind {
        RowKind::FileHeader => style(clip_to_width(&row.text(), width)).bold().to_string(),
        RowKind::Hunk => style(clip_to_width(&row.text(), width))
            .color256(DIFF_HUNK_FG)
            .to_string(),
        RowKind::Meta => style(clip_to_width(&row.text(), width)).dim().to_string(),
        RowKind::Context | RowKind::Add | RowKind::Del => {
            let gutter = diff_gutter(row.old_no, row.new_no, num_w);
            let gutter_w = num_w * 2 + 2;
            let marker = match row.kind {
                RowKind::Add => '+',
                RowKind::Del => '-',
                _ => ' ',
            };
            let (base_bg, emph_bg) = diff_backgrounds(row.kind);
            // Budget: the pane width less the gutter and the one-column marker.
            let budget = width.saturating_sub(gutter_w + 1);
            let marker_styled = match base_bg {
                Some(bg) => style(marker.to_string()).on_color256(bg).to_string(),
                None => marker.to_string(),
            };
            let content = diff_content(&row.spans, &row.changed, base_bg, emph_bg, budget);
            format!("{gutter}{marker_styled}{content}")
        }
    }
}

/// Render one side-by-side row: a full-width header, or old (left) and new
/// (right) columns separated by a dim bar, each a fixed `col_w` wide so the two
/// halves always line up.
fn diff_split_row(
    doc: &crate::presentation::tui::diff::DiffDoc,
    row: SplitRow,
    num_w: usize,
    width: usize,
) -> String {
    match row {
        SplitRow::Full(i) => diff_unified_row(&doc.rows[i], num_w, width),
        SplitRow::Pair { left, right } => {
            let col_w = width.saturating_sub(1) / 2;
            let left = diff_half(left.map(|i| &doc.rows[i]), true, num_w, col_w);
            let right = diff_half(right.map(|i| &doc.rows[i]), false, num_w, col_w);
            let sep = style("│").dim().to_string();
            format!("{left}{sep}{right}")
        }
    }
}

/// One column of the split layout: a single line number + the content on its
/// tint, padded to exactly `col_w`. An absent row (the short side of a replaced
/// block) renders as blank padding.
fn diff_half(row: Option<&DiffRow>, is_left: bool, num_w: usize, col_w: usize) -> String {
    let Some(row) = row else {
        return " ".repeat(col_w);
    };
    let no = if is_left { row.old_no } else { row.new_no };
    let num = no.map(|n| n.to_string()).unwrap_or_default();
    let gutter = format!("{num:>num_w$} ");
    let gutter_styled = style(&gutter).color256(DIFF_NUM_FG).to_string();
    let (base_bg, emph_bg) = diff_backgrounds(row.kind);
    let budget = col_w.saturating_sub(num_w + 1);
    let content = diff_content(&row.spans, &row.changed, base_bg, emph_bg, budget);
    format!("{gutter_styled}{content}")
}

/// The `old new ` line-number gutter for the unified layout (each number
/// right-aligned in `num_w` columns), dimmed and blank where a number is absent.
fn diff_gutter(old: Option<usize>, new: Option<usize>, num_w: usize) -> String {
    let old = old.map(|n| n.to_string()).unwrap_or_default();
    let new = new.map(|n| n.to_string()).unwrap_or_default();
    style(format!("{old:>num_w$} {new:>num_w$} "))
        .color256(DIFF_NUM_FG)
        .to_string()
}

/// The base and word-emphasis background tints for a row kind (`None` for context
/// and headers, which take no background).
fn diff_backgrounds(kind: RowKind) -> (Option<u8>, Option<u8>) {
    match kind {
        RowKind::Add => (Some(DIFF_ADD_BG), Some(DIFF_ADD_EMPH_BG)),
        RowKind::Del => (Some(DIFF_DEL_BG), Some(DIFF_DEL_EMPH_BG)),
        _ => (None, None),
    }
}

/// Render a content line's spans into `budget` display columns: each character
/// keeps its syntax-highlight foreground, sits on the base tint, and switches to
/// the brighter emphasis tint inside a `changed` word range. Runs of like-styled
/// characters are coalesced, the line is clipped to the budget, and — when a
/// background is set — the remaining columns are padded so the tint fills the row.
fn diff_content(
    spans: &[DiffSpan],
    changed: &[(usize, usize)],
    base_bg: Option<u8>,
    emph_bg: Option<u8>,
    budget: usize,
) -> String {
    let mut out = String::new();
    let mut col = 0usize; // display columns emitted
    let mut idx = 0usize; // char index into the content (for `changed`)
    let mut run = String::new();
    let mut run_fg: Option<Rgb> = None;
    let mut run_emph = false;

    for span in spans {
        for ch in span.text.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if col + w > budget {
                push_diff_run(&mut out, &run, run_fg, run_emph, base_bg, emph_bg);
                run.clear();
                return pad_diff(out, col, budget, base_bg);
            }
            let emph = in_changed(idx, changed);
            if !run.is_empty() && (span.color != run_fg || emph != run_emph) {
                push_diff_run(&mut out, &run, run_fg, run_emph, base_bg, emph_bg);
                run.clear();
            }
            if run.is_empty() {
                run_fg = span.color;
                run_emph = emph;
            }
            run.push(ch);
            col += w;
            idx += 1;
        }
    }
    push_diff_run(&mut out, &run, run_fg, run_emph, base_bg, emph_bg);
    pad_diff(out, col, budget, base_bg)
}

/// Append a coalesced run of like-styled characters to `out`, applying its
/// foreground colour and its background (the emphasis tint when `emph`, else the
/// base tint). A run with no background and no colour is emitted verbatim.
fn push_diff_run(
    out: &mut String,
    run: &str,
    fg: Option<Rgb>,
    emph: bool,
    base_bg: Option<u8>,
    emph_bg: Option<u8>,
) {
    if run.is_empty() {
        return;
    }
    let bg = if emph { emph_bg } else { base_bg };
    let mut styled = style(run.to_string());
    if let Some(fg) = fg {
        styled = styled.color256(rgb_to_ansi256(fg));
    }
    if let Some(bg) = bg {
        styled = styled.on_color256(bg);
    }
    out.push_str(&styled.to_string());
}

/// Pad the emitted content out to `budget` columns so a set background tint fills
/// the whole row (GitHub-style); with no background the row is left as-is.
fn pad_diff(mut out: String, col: usize, budget: usize, base_bg: Option<u8>) -> String {
    if col < budget {
        let pad = " ".repeat(budget - col);
        match base_bg {
            Some(bg) => out.push_str(&style(pad).on_color256(bg).to_string()),
            None => out.push_str(&pad),
        }
    }
    out
}

/// Whether char index `idx` falls inside any half-open `changed` range.
fn in_changed(idx: usize, changed: &[(usize, usize)]) -> bool {
    changed.iter().any(|&(s, e)| idx >= s && idx < e)
}

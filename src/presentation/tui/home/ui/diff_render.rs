//! Rendering for the right-pane diff viewer.

use console::style;
use unicode_width::UnicodeWidthChar;

use super::super::state::DiffView;
use super::clip_to_width;
use super::markdown_render::rgb_to_ansi256;
use crate::presentation::tui::diff::{split_rows, DiffRow, DiffSpan, RowKind, SplitRow};
use crate::presentation::tui::markdown::Rgb;

const DIFF_ADD_BG: u8 = 22; // dark green
const DIFF_ADD_EMPH_BG: u8 = 28; // brighter green
const DIFF_DEL_BG: u8 = 52; // dark red
const DIFF_DEL_EMPH_BG: u8 = 88; // brighter red
const DIFF_NUM_FG: u8 = 244; // dim grey line numbers
const DIFF_HUNK_FG: u8 = 37; // teal hunk headers

/// Render the right-pane diff view: a one-row header (title + scroll position)
/// over a window of diff rows, laid out unified or side-by-side. The window is
/// clamped so the last row stays in view (matching the event loop's scroll
/// clamp), and every row is exactly `width` columns so the layout never shifts.
pub(super) fn diff_pane(view: &DiffView, width: usize, rows: usize) -> Vec<String> {
    let body_h = rows.saturating_sub(1);
    // An empty patch (a session that changed nothing) shows a friendly line
    // instead of a blank pane.
    if view.doc.is_empty() {
        let mut lines = Vec::with_capacity(rows);
        lines.push(diff_header(view, 0, 0, 0, width));
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
    }

    let num_w = num_width(&view.doc);
    // The renderable rows depend on the layout: split folds paired add/del lines
    // into one visual row.
    let split = view.split.then(|| split_rows(&view.doc));
    let total = split.as_ref().map_or(view.doc.rows.len(), Vec::len);
    let max_start = total.saturating_sub(body_h);
    let start = view.scroll.min(max_start);
    let end = (start + body_h).min(total);

    let mut lines = Vec::with_capacity(rows);
    lines.push(diff_header(view, start, end, total, width));
    if rows <= 1 {
        lines.truncate(rows);
        return lines;
    }
    for i in 0..body_h {
        let row = match &split {
            Some(split) => split
                .get(start + i)
                .map(|sr| diff_split_row(&view.doc, *sr, num_w, width)),
            None => view
                .doc
                .rows
                .get(start + i)
                .map(|r| diff_unified_row(r, num_w, width)),
        };
        lines.push(row.unwrap_or_default());
    }
    lines
}

/// The diff view's one-row header: an icon, the branch → base title, the layout
/// name, and a `start-end/total` position once it scrolls.
fn diff_header(view: &DiffView, start: usize, end: usize, total: usize, width: usize) -> String {
    let layout = if view.split { "split" } else { "unified" };
    let header = if total > end.saturating_sub(start) && total > 0 {
        format!(
            " {}  [{}]  ({}-{}/{})",
            view.title,
            layout,
            start + 1,
            end,
            total
        )
    } else {
        format!(" {}  [{}]", view.title, layout)
    };
    style(clip_to_width(&header, width)).bold().to_string()
}

/// The line-number gutter width: the digit count of the largest line number in
/// the diff (at least 2), so the gutter is as narrow as the content allows.
fn num_width(doc: &crate::presentation::tui::diff::DiffDoc) -> usize {
    let max = doc
        .rows
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

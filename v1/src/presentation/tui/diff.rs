//! A structured model of a unified `git diff`, for the home screen's rich,
//! GitHub-style diff view.
//!
//! [`render`] parses the patch text into a [`DiffDoc`] — a flat list of
//! [`DiffRow`]s, each carrying its kind (file header / hunk / context / add /
//! del), its old/new line numbers, and its content already **syntax-highlighted**
//! by the file's language (reusing [`markdown::highlight`]). Added/removed line
//! pairs additionally get **word-level** change ranges via [`similar`], so the UI
//! can emphasise just the parts that actually changed — the way GitHub does.
//!
//! The result is **pure data**: no terminal escapes are produced here, so the
//! parsing, highlighting, and word-diffing are all directly testable. Turning a
//! [`DiffRow`] into a styled terminal row (gutter, background tint, emphasis) is
//! the UI layer's job (see the home screen's `panes` module), which also chooses
//! between the unified and split (side-by-side) layouts from the same rows.

use std::collections::HashMap;

use similar::{ChangeTag, TextDiff};

use super::markdown::{highlight, Rgb};

/// Half-open `[start, end)` char ranges within a line's content — the changed
/// spans a word-level diff marks for emphasis.
type Ranges = Vec<(usize, usize)>;

/// The kind of a diff row, which governs its colour and how the split layout
/// places it (context on both sides, add on the right, del on the left).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// The `diff --git a/… b/…` banner that opens each file's section.
    FileHeader,
    /// A `@@ -a,b +c,d @@` hunk header.
    Hunk,
    /// A non-content header line (`index`, `---`, `+++`, mode / rename notices,
    /// the binary-file notice, the "no newline" marker).
    Meta,
    /// An unchanged context line (present on both sides).
    Context,
    /// An added line (right side only).
    Add,
    /// A removed line (left side only).
    Del,
}

/// A run of text within a diff row's content, carrying the syntax-highlight
/// foreground colour of its tokens (`None` for an unhighlighted header line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSpan {
    pub text: String,
    pub color: Option<Rgb>,
}

/// One parsed diff row: its kind, old/new line numbers (each present only where
/// meaningful), the syntax-highlighted content spans (without the leading
/// `+`/`-`/space marker), and — for add/del rows paired by [`word_diff`] — the
/// char ranges within the content that changed, for word-level emphasis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRow {
    pub kind: RowKind,
    pub old_no: Option<usize>,
    pub new_no: Option<usize>,
    pub spans: Vec<DiffSpan>,
    /// Half-open `[start, end)` char ranges (in the content) that changed,
    /// relative to the paired line on the other side. Empty for context/headers
    /// and for add/del lines that could not be paired.
    pub changed: Vec<(usize, usize)>,
}

impl DiffRow {
    /// The row's content with highlighting dropped — its spans concatenated.
    /// Handy for width measurement, word-diffing, and tests.
    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }
}

/// A parsed unified diff: a flat list of rows in file order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffDoc {
    pub rows: Vec<DiffRow>,
}

impl DiffDoc {
    /// Whether the diff has no content rows (no add/del/context) — an empty patch.
    pub fn is_empty(&self) -> bool {
        !self
            .rows
            .iter()
            .any(|r| matches!(r.kind, RowKind::Context | RowKind::Add | RowKind::Del))
    }
}

/// One visual row of the side-by-side (split) layout, referencing rows by index.
/// A header/hunk/meta row spans both columns ([`Full`]); a content row places old
/// (removed/context) on the left and new (added/context) on the right, either of
/// which may be absent when a replaced block is longer on one side.
///
/// [`Full`]: SplitRow::Full
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitRow {
    /// A header / hunk / meta row, drawn across the full width.
    Full(usize),
    /// A content row: `left` (old side: context or removed) and `right` (new side:
    /// context or added), by row index. At least one is `Some`.
    Pair {
        left: Option<usize>,
        right: Option<usize>,
    },
}

/// Fold the flat rows into the side-by-side layout: context rows sit on both
/// sides, a removed run aligns line-by-line with the added run that follows it
/// (surplus lines on either side occupy their own half-empty row), and headers
/// span the full width. The mapping is index-based so the renderer and the scroll
/// clamp share one definition of "how many visual rows the split layout has".
pub fn split_rows(doc: &DiffDoc) -> Vec<SplitRow> {
    split_rows_slice(&doc.rows, 0)
}

/// Fold a *slice* of rows into the side-by-side layout, offsetting every emitted
/// row index by `base` so the returned [`SplitRow`]s still reference the rows by
/// their position in the whole [`DiffDoc`]. This lets the diff view fold only the
/// selected file's section (`doc.rows[start..end]`) while the renderer keeps
/// indexing the shared `doc`. [`split_rows`] is the whole-document case
/// (`base == 0`).
pub fn split_rows_slice(rows: &[DiffRow], base: usize) -> Vec<SplitRow> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        match rows[i].kind {
            RowKind::Context => {
                out.push(SplitRow::Pair {
                    left: Some(base + i),
                    right: Some(base + i),
                });
                i += 1;
            }
            RowKind::Del => {
                let del_start = i;
                while i < rows.len() && rows[i].kind == RowKind::Del {
                    i += 1;
                }
                let add_start = i;
                while i < rows.len() && rows[i].kind == RowKind::Add {
                    i += 1;
                }
                let dels = add_start - del_start;
                let adds = i - add_start;
                for k in 0..dels.max(adds) {
                    out.push(SplitRow::Pair {
                        left: (k < dels).then_some(base + del_start + k),
                        right: (k < adds).then_some(base + add_start + k),
                    });
                }
            }
            RowKind::Add => {
                // An added run with no preceding removed run (a pure insertion):
                // each added line occupies the right side only.
                out.push(SplitRow::Pair {
                    left: None,
                    right: Some(base + i),
                });
                i += 1;
            }
            _ => {
                out.push(SplitRow::Full(base + i));
                i += 1;
            }
        }
    }
    out
}

/// One changed file's section within a [`DiffDoc`]: its display path, the added /
/// removed line counts (for the explorer's `+N -M` badge), and the half-open
/// `[start, end)` range of `doc.rows` that belongs to it — from its `diff --git`
/// header up to the next file's (or the end of the document). The diff view shows
/// exactly this range on its right side when the file is selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFile {
    pub path: String,
    pub added: usize,
    pub removed: usize,
    pub start: usize,
    pub end: usize,
}

/// The changed files of a parsed diff, in file order, one per
/// [`RowKind::FileHeader`] row. Each carries the row range of its section and its
/// add / remove counts. A patch with no file headers (empty, or headerless) yields
/// no files.
pub fn files(doc: &DiffDoc) -> Vec<DiffFile> {
    let headers: Vec<usize> = doc
        .rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.kind == RowKind::FileHeader)
        .map(|(i, _)| i)
        .collect();
    headers
        .iter()
        .enumerate()
        .map(|(n, &start)| {
            let end = headers.get(n + 1).copied().unwrap_or(doc.rows.len());
            let section = &doc.rows[start..end];
            let added = section.iter().filter(|r| r.kind == RowKind::Add).count();
            let removed = section.iter().filter(|r| r.kind == RowKind::Del).count();
            DiffFile {
                path: section_path(section),
                added,
                removed,
                start,
                end,
            }
        })
        .collect()
}

/// The display path for a file section: the new path from its `+++ b/<path>` line
/// (the side GitHub names files by), falling back to the old `--- a/<path>` for a
/// deletion, and finally to the `b/` operand of the `diff --git` banner when a
/// section carries neither (a pure rename or a binary file).
fn section_path(section: &[DiffRow]) -> String {
    let meta = |prefix: &str| {
        section.iter().find_map(|r| {
            let text = r.text();
            text.strip_prefix(prefix).map(clean_path)
        })
    };
    let new = meta("+++ ").filter(|p| !p.is_empty());
    let old = meta("--- ").filter(|p| !p.is_empty());
    new.or(old)
        .or_else(|| section.first().and_then(|r| header_path(&r.text())))
        .unwrap_or_default()
}

/// The `b/<path>` operand of a `diff --git a/<old> b/<new>` banner. Splits on the
/// ` b/` boundary so the new path is preferred; returns `None` when the banner is
/// malformed.
fn header_path(header: &str) -> Option<String> {
    let rest = header.strip_prefix("diff --git ")?;
    rest.find(" b/")
        .map(|at| clean_path(&rest[at + 1..]))
        .filter(|p| !p.is_empty())
}

/// Strip an `a/` / `b/` prefix and any trailing tab-separated git metadata from a
/// diff path operand, mapping `/dev/null` to the empty string (an absent side).
fn clean_path(operand: &str) -> String {
    let path = operand
        .split('\t')
        .next()
        .unwrap_or(operand)
        .trim_start_matches("a/")
        .trim_start_matches("b/");
    if path == "/dev/null" {
        String::new()
    } else {
        path.to_string()
    }
}

/// What a [`TreeRow`] stands for: a directory (carrying its full path, the key the
/// diff view collapses it by) or a changed file (carrying its index into
/// [`files`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeKind {
    /// A directory node, keyed by its full slash-joined path (e.g. `src/ui`).
    Dir { path: String },
    /// A file leaf, by its index into the [`files`] list.
    File { index: usize },
}

/// One row of the explorer's directory tree: its indentation `depth`, the path
/// segment `name` shown at that depth, and what it stands for ([`TreeKind`]). The
/// rows are a depth-first flattening of the changed files' paths, directories
/// before files and alphabetical within each — GitHub's file-tree order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    pub depth: usize,
    pub name: String,
    pub kind: TreeKind,
}

/// An intermediate directory node used only while building the tree: its children
/// in insertion order (later sorted), split into subdirectories and file leaves.
#[derive(Default)]
struct DirNode {
    order: Vec<String>,
    dirs: HashMap<String, DirNode>,
    files: HashMap<String, usize>,
}

impl DirNode {
    fn child(&mut self, segment: &str) -> &mut DirNode {
        if !self.order.contains(&segment.to_string()) {
            self.order.push(segment.to_string());
        }
        self.dirs.entry(segment.to_string()).or_default()
    }

    fn leaf(&mut self, segment: &str, index: usize) {
        if !self.order.contains(&segment.to_string()) {
            self.order.push(segment.to_string());
        }
        self.files.insert(segment.to_string(), index);
    }
}

/// Flatten the changed `files`' paths into an explorer directory tree: each path
/// is split on `/`, its leading segments become nested directory nodes and its
/// last segment a file leaf. The result is depth-first, directories before files
/// and alphabetical within each level, so the explorer reads like GitHub's file
/// tree. A file with no `/` sits at depth 0.
pub fn tree_rows(files: &[DiffFile]) -> Vec<TreeRow> {
    let mut root = DirNode::default();
    for (index, file) in files.iter().enumerate() {
        let mut segments = file.path.split('/').filter(|s| !s.is_empty()).peekable();
        let mut node = &mut root;
        // Empty path (a binary/rename with no resolvable name): fall back to a
        // single leaf named by the raw path so it is still selectable.
        if segments.peek().is_none() {
            node.leaf(&file.path, index);
            continue;
        }
        while let Some(segment) = segments.next() {
            if segments.peek().is_some() {
                node = node.child(segment);
            } else {
                node.leaf(segment, index);
            }
        }
    }
    let mut out = Vec::new();
    emit_tree(&root, 0, "", &mut out);
    out
}

/// Depth-first emit of a directory node's children into `out`: directories first
/// (recursing into each), then file leaves, both alphabetical, so the flattened
/// rows match the on-screen tree order.
fn emit_tree(node: &DirNode, depth: usize, prefix: &str, out: &mut Vec<TreeRow>) {
    let mut segments = node.order.clone();
    segments.sort_by(|a, b| {
        let a_dir = node.dirs.contains_key(a);
        let b_dir = node.dirs.contains_key(b);
        b_dir.cmp(&a_dir).then_with(|| a.cmp(b))
    });
    for segment in segments {
        if let Some(sub) = node.dirs.get(&segment) {
            let path = if prefix.is_empty() {
                segment.clone()
            } else {
                format!("{prefix}/{segment}")
            };
            out.push(TreeRow {
                depth,
                name: segment.clone(),
                kind: TreeKind::Dir { path: path.clone() },
            });
            emit_tree(sub, depth + 1, &path, out);
        } else if let Some(&index) = node.files.get(&segment) {
            out.push(TreeRow {
                depth,
                name: segment,
                kind: TreeKind::File { index },
            });
        }
    }
}

/// The most rows [`render`] emits, bounding work and allocation on a pathological
/// patch (mirrors the Markdown renderer's cap).
const MAX_ROWS: usize = 20_000;

/// Parse a unified `diff` (as produced by `git diff`) into a [`DiffDoc`],
/// syntax-highlighting content by each file's language and computing word-level
/// change ranges for paired add/del lines.
pub fn render(diff: &str) -> DiffDoc {
    // An empty patch has no rows (`"".split('\n')` would otherwise yield one
    // spurious blank line). A single trailing newline is dropped too — git's diff
    // ends with one, and it must not become a trailing empty context row — while
    // blank lines *within* the diff (a blank context line) are kept.
    if diff.is_empty() {
        return DiffDoc::default();
    }
    let diff = diff.strip_suffix('\n').unwrap_or(diff);
    let mut rows: Vec<DiffRow> = Vec::new();
    // The current file's language token, taken from its `+++ b/<path>` (or the
    // `--- a/<path>` fallback for a deletion), used to highlight its content.
    let mut lang = String::new();
    let mut old_no: usize = 0;
    let mut new_no: usize = 0;

    for raw in diff.split('\n').take(MAX_ROWS) {
        let line = raw.strip_suffix('\r').unwrap_or(raw);

        if line.starts_with("diff --git") {
            rows.push(header_row(RowKind::FileHeader, line));
            continue;
        }
        // `+++ b/<path>` names the new file; `--- a/<path>` the old. Prefer the
        // new path for the language, falling back to the old (pure deletions).
        if let Some(path) = line.strip_prefix("+++ ") {
            lang = lang_token(path);
            rows.push(header_row(RowKind::Meta, line));
            continue;
        }
        if let Some(path) = line.strip_prefix("--- ") {
            if lang.is_empty() {
                lang = lang_token(path);
            }
            rows.push(header_row(RowKind::Meta, line));
            continue;
        }
        if let Some((o, n)) = parse_hunk(line) {
            old_no = o;
            new_no = n;
            rows.push(header_row(RowKind::Hunk, line));
            continue;
        }
        if is_meta(line) {
            rows.push(header_row(RowKind::Meta, line));
            continue;
        }
        // Content lines. The first byte is the marker; the rest is the code,
        // highlighted by the current file language.
        if let Some(code) = line.strip_prefix('+') {
            rows.push(content_row(RowKind::Add, code, &lang, None, Some(new_no)));
            new_no += 1;
        } else if let Some(code) = line.strip_prefix('-') {
            rows.push(content_row(RowKind::Del, code, &lang, Some(old_no), None));
            old_no += 1;
        } else {
            // A context line (leading space) or an empty tail line.
            let code = line.strip_prefix(' ').unwrap_or(line);
            rows.push(content_row(
                RowKind::Context,
                code,
                &lang,
                Some(old_no),
                Some(new_no),
            ));
            old_no += 1;
            new_no += 1;
        }
    }

    word_diff(&mut rows);
    DiffDoc { rows }
}

/// A header/meta/hunk row: the raw line as a single uncoloured span (the UI
/// colours it by [`RowKind`]), with no line numbers.
fn header_row(kind: RowKind, line: &str) -> DiffRow {
    DiffRow {
        kind,
        old_no: None,
        new_no: None,
        spans: vec![DiffSpan {
            text: line.to_string(),
            color: None,
        }],
        changed: Vec::new(),
    }
}

/// A content row, syntax-highlighting `code` by `lang` (an empty/unknown language
/// falls back to a single uncoloured span).
fn content_row(
    kind: RowKind,
    code: &str,
    lang: &str,
    old_no: Option<usize>,
    new_no: Option<usize>,
) -> DiffRow {
    // Reuse the Markdown code highlighter: one line in, one run of coloured spans
    // out. Tabs are expanded to spaces there, keeping the rendered width honest.
    let spans = highlight::highlight_block(&[code], lang)
        .into_iter()
        .next()
        .map(|line| {
            line.into_iter()
                .map(|span| DiffSpan {
                    text: span.text,
                    color: span.color,
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    // An empty line highlights to no spans; keep one empty span so the row still
    // has content to align a background against.
    let spans = if spans.is_empty() {
        vec![DiffSpan {
            text: String::new(),
            color: None,
        }]
    } else {
        spans
    };
    DiffRow {
        kind,
        old_no,
        new_no,
        spans,
        changed: Vec::new(),
    }
}

/// Fill in word-level `changed` ranges by pairing each run of removed lines with
/// the run of added lines that immediately follows it, line by line. Only the
/// overlapping prefix (`min(dels, adds)` lines) is paired; surplus lines on
/// either side stay wholly-changed (no intra-line emphasis), matching how
/// line-oriented diff viewers align a replaced block.
fn word_diff(rows: &mut [DiffRow]) {
    let mut i = 0;
    while i < rows.len() {
        if rows[i].kind != RowKind::Del {
            i += 1;
            continue;
        }
        let del_start = i;
        while i < rows.len() && rows[i].kind == RowKind::Del {
            i += 1;
        }
        let add_start = i;
        while i < rows.len() && rows[i].kind == RowKind::Add {
            i += 1;
        }
        let dels = add_start - del_start;
        let adds = i - add_start;
        for k in 0..dels.min(adds) {
            let old_text = rows[del_start + k].text();
            let new_text = rows[add_start + k].text();
            let (del_ranges, add_ranges) = word_ranges(&old_text, &new_text);
            rows[del_start + k].changed = del_ranges;
            rows[add_start + k].changed = add_ranges;
        }
    }
}

/// The char ranges that changed on each side of a replaced line pair, computed
/// with a word-level [`similar`] diff. Returns `(old_side_ranges,
/// new_side_ranges)` as half-open `[start, end)` char offsets.
fn word_ranges(old: &str, new: &str) -> (Ranges, Ranges) {
    let diff = TextDiff::from_words(old, new);
    let mut old_pos = 0usize;
    let mut new_pos = 0usize;
    let mut old_ranges: Ranges = Vec::new();
    let mut new_ranges: Ranges = Vec::new();
    for change in diff.iter_all_changes() {
        let len = change.value().chars().count();
        match change.tag() {
            ChangeTag::Equal => {
                old_pos += len;
                new_pos += len;
            }
            ChangeTag::Delete => {
                push_range(&mut old_ranges, old_pos, old_pos + len);
                old_pos += len;
            }
            ChangeTag::Insert => {
                push_range(&mut new_ranges, new_pos, new_pos + len);
                new_pos += len;
            }
        }
    }
    (old_ranges, new_ranges)
}

/// Append `[start, end)` to `ranges`, merging it into the previous range when
/// they touch so adjacent changed tokens render as one highlighted run. Called
/// only with non-empty ranges (`similar`'s word tokens each carry ≥1 char).
fn push_range(ranges: &mut Ranges, start: usize, end: usize) {
    match ranges.last_mut() {
        Some(last) if last.1 == start => last.1 = end,
        _ => ranges.push((start, end)),
    }
}

/// Parse a `@@ -old[,n] +new[,m] @@` hunk header into its `(old, new)` starting
/// line numbers, or `None` when the line is not a hunk header.
fn parse_hunk(line: &str) -> Option<(usize, usize)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(' ')?;
    let new = rest.strip_prefix('+')?;
    let new = new.split_once(' ').map(|(n, _)| n).unwrap_or(new);
    let old_start = old.split(',').next()?.parse().ok()?;
    let new_start = new.split(',').next()?.parse().ok()?;
    Some((old_start, new_start))
}

/// Whether `line` is a non-content header line other than the `diff --git` banner
/// and the `+++`/`---` markers (which the caller handles first for the language).
fn is_meta(line: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "index ",
        "old mode",
        "new mode",
        "deleted file",
        "new file",
        "copy from",
        "copy to",
        "rename from",
        "rename to",
        "similarity index",
        "dissimilarity index",
        "Binary files",
        "\\ No newline",
    ];
    PREFIXES.iter().any(|p| line.starts_with(p))
}

/// The language token for a `+++ b/<path>` / `--- a/<path>` operand: the file's
/// extension (e.g. `src/main.rs` → `rs`), which the highlighter's alias table
/// resolves to a syntax. `/dev/null` and a path without an extension yield an
/// empty token (plain, uncoloured content).
fn lang_token(operand: &str) -> String {
    // Drop the `a/` or `b/` prefix and any trailing tab-separated metadata git
    // appends to the file line.
    let path = operand
        .split('\t')
        .next()
        .unwrap_or(operand)
        .trim_start_matches("a/")
        .trim_start_matches("b/");
    if path == "/dev/null" {
        return String::new();
    }
    path.rsplit_once('.')
        .map(|(_, ext)| ext.to_lowercase())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small multi-file-ish patch exercising every row kind.
    const PATCH: &str = "diff --git a/src/main.rs b/src/main.rs\n\
index 111..222 100644\n\
--- a/src/main.rs\n\
+++ b/src/main.rs\n\
@@ -1,3 +1,3 @@\n\
 fn main() {\n\
-    let value = old_thing;\n\
+    let value = new_thing;\n\
 }";

    fn kinds(doc: &DiffDoc) -> Vec<RowKind> {
        doc.rows.iter().map(|r| r.kind).collect()
    }

    #[test]
    fn parses_every_row_kind_in_order() {
        let doc = render(PATCH);
        assert_eq!(
            kinds(&doc),
            vec![
                RowKind::FileHeader, // diff --git
                RowKind::Meta,       // index
                RowKind::Meta,       // ---
                RowKind::Meta,       // +++
                RowKind::Hunk,       // @@
                RowKind::Context,    // fn main() {
                RowKind::Del,        // - let value = old_thing;
                RowKind::Add,        // + let value = new_thing;
                RowKind::Context,    // }
            ]
        );
        assert!(!doc.is_empty());
    }

    #[test]
    fn content_rows_track_old_and_new_line_numbers() {
        let doc = render(PATCH);
        let context_open = &doc.rows[5];
        assert_eq!(context_open.old_no, Some(1));
        assert_eq!(context_open.new_no, Some(1));
        // The removed line advances only the old counter; the added only the new.
        let del = &doc.rows[6];
        assert_eq!(del.old_no, Some(2));
        assert_eq!(del.new_no, None);
        let add = &doc.rows[7];
        assert_eq!(add.old_no, None);
        assert_eq!(add.new_no, Some(2));
        // The trailing context line resumes at old 3 / new 3.
        let context_close = &doc.rows[8];
        assert_eq!(context_close.old_no, Some(3));
        assert_eq!(context_close.new_no, Some(3));
    }

    #[test]
    fn content_is_stripped_of_the_marker_and_highlighted() {
        let doc = render(PATCH);
        // The del/add rows keep the code without the leading +/- marker.
        assert_eq!(doc.rows[6].text(), "    let value = old_thing;");
        assert_eq!(doc.rows[7].text(), "    let value = new_thing;");
        // Rust highlighting splits the line into several coloured spans.
        assert!(doc.rows[7].spans.len() > 1);
        assert!(doc.rows[7].spans.iter().any(|s| s.color.is_some()));
    }

    #[test]
    fn word_diff_marks_only_the_changed_token() {
        let doc = render(PATCH);
        let del = &doc.rows[6];
        let add = &doc.rows[7];
        // Both sides mark a change, and it is localized (not the whole line).
        assert!(!del.changed.is_empty());
        assert!(!add.changed.is_empty());
        // The shared prefix "    let value = " is unchanged, so the first changed
        // range starts past it, and the marked text is the differing identifier.
        let add_text = add.text();
        let (start, end) = add.changed[0];
        assert!(start >= "    let value = ".chars().count(), "start={start}");
        let marked: String = add_text.chars().skip(start).take(end - start).collect();
        assert!(marked.contains("new_thing"), "marked={marked:?}");
    }

    #[test]
    fn an_empty_content_line_keeps_one_blank_span() {
        // A blank added line highlights to no spans; the row keeps one empty span
        // so a background can still be aligned against it.
        let doc = render("@@ -0,0 +1 @@\n+\n");
        let add = doc.rows.iter().find(|r| r.kind == RowKind::Add).unwrap();
        assert_eq!(add.spans.len(), 1);
        assert_eq!(add.text(), "");
    }

    #[test]
    fn unpaired_added_lines_get_no_word_ranges() {
        // A pure addition (no removed line to pair with) has no intra-line marks.
        let doc = render("@@ -0,0 +1 @@\n+brand new line\n");
        let add = doc.rows.iter().find(|r| r.kind == RowKind::Add).unwrap();
        assert!(add.changed.is_empty());
    }

    #[test]
    fn surplus_replaced_lines_pair_by_prefix_only() {
        // Two removed, one added: only the first pair word-diffs; the surplus
        // removed line keeps no intra-line marks.
        let doc = render("@@ -1,2 +1 @@\n-alpha one\n-beta two\n+alpha ONE\n");
        let dels: Vec<&DiffRow> = doc.rows.iter().filter(|r| r.kind == RowKind::Del).collect();
        assert_eq!(dels.len(), 2);
        assert!(!dels[0].changed.is_empty()); // paired with the added line
        assert!(dels[1].changed.is_empty()); // surplus, unpaired
    }

    #[test]
    fn empty_patch_and_headers_only_report_empty() {
        assert!(render("").is_empty());
        // Headers with no content rows are still "empty" (nothing changed shown).
        assert!(render("diff --git a/f b/f\nindex 1..2\n").is_empty());
    }

    #[test]
    fn hunk_headers_parse_with_and_without_counts() {
        // `@@ -old[,n] +new[,m] @@` → (old_start, new_start).
        assert_eq!(parse_hunk("@@ -5,3 +8,4 @@ fn main"), Some((5, 8)));
        assert_eq!(parse_hunk("@@ -10 +12 @@"), Some((10, 12)));
        assert_eq!(parse_hunk("not a hunk"), None);
        assert_eq!(parse_hunk("@@ -x +1 @@"), None);
    }

    #[test]
    fn lang_token_extracts_the_extension() {
        assert_eq!(lang_token("b/src/main.rs"), "rs");
        assert_eq!(lang_token("a/docs/README.MD"), "md");
        assert_eq!(lang_token("/dev/null"), "");
        assert_eq!(lang_token("b/Makefile"), ""); // no extension
                                                  // git may append tab-separated metadata to the file operand.
        assert_eq!(lang_token("b/src/x.rs\t(new)"), "rs");
    }

    #[test]
    fn adjacent_changed_tokens_merge_into_one_range() {
        // Deleting several consecutive tokens ("a", " ", "b", " ") collapses into
        // a single highlighted run rather than one range per token.
        let (old, _new) = word_ranges("a b c", "c");
        assert_eq!(old, vec![(0, 4)]);
    }

    #[test]
    fn a_pathological_patch_is_capped() {
        let huge = "+x\n".repeat(MAX_ROWS + 500);
        assert_eq!(render(&huge).rows.len(), MAX_ROWS);
    }

    #[test]
    fn split_rows_align_context_both_sides_and_pair_replacements() {
        // The PATCH: 4 headers, a context line, a del/add replacement, a context.
        let doc = render(PATCH);
        let split = split_rows(&doc);
        // 5 header rows (diff/index/---/+++/@@, each full width) + 3 content rows
        // (2 context + 1 paired del/add).
        assert_eq!(split.len(), 8);
        assert!(matches!(split[0], SplitRow::Full(0)));
        // The context line sits on both sides.
        assert!(matches!(
            split[5],
            SplitRow::Pair {
                left: Some(_),
                right: Some(_)
            }
        ));
        // The del(6)/add(7) replacement pairs into a single split row.
        assert_eq!(
            split[6],
            SplitRow::Pair {
                left: Some(6),
                right: Some(7)
            }
        );
    }

    #[test]
    fn split_rows_place_surplus_and_pure_insertions_on_one_side() {
        // Two removed, one added: the paired row carries both, the surplus removed
        // line occupies the left side only.
        let doc = render("@@ -1,2 +1 @@\n-a one\n-b two\n+a ONE\n");
        let split = split_rows(&doc);
        let content: Vec<SplitRow> = split
            .into_iter()
            .filter(|r| matches!(r, SplitRow::Pair { .. }))
            .collect();
        assert_eq!(content.len(), 2);
        assert!(matches!(
            content[0],
            SplitRow::Pair {
                left: Some(_),
                right: Some(_)
            }
        ));
        assert!(matches!(
            content[1],
            SplitRow::Pair {
                left: Some(_),
                right: None
            }
        ));

        // A pure insertion sits on the right side only.
        let ins = render("@@ -0,0 +1 @@\n+fresh\n");
        let split = split_rows(&ins);
        assert!(split.iter().any(|r| matches!(
            r,
            SplitRow::Pair {
                left: None,
                right: Some(_)
            }
        )));
    }

    /// A three-file patch across two directories: an edit, an addition, and a
    /// deletion — the shape the explorer tree groups.
    const MULTI: &str = "diff --git a/src/main.rs b/src/main.rs\n\
index 111..222 100644\n\
--- a/src/main.rs\n\
+++ b/src/main.rs\n\
@@ -1,2 +1,2 @@\n\
 fn main() {\n\
-    old();\n\
+    new();\n\
diff --git a/src/ui/render.rs b/src/ui/render.rs\n\
new file mode 100644\n\
index 000..333\n\
--- /dev/null\n\
+++ b/src/ui/render.rs\n\
@@ -0,0 +1,2 @@\n\
+one\n\
+two\n\
diff --git a/README.md b/README.md\n\
deleted file mode 100644\n\
index 444..000\n\
--- a/README.md\n\
+++ /dev/null\n\
@@ -1 +0,0 @@\n\
-gone\n";

    #[test]
    fn files_lists_each_section_with_counts_and_row_ranges() {
        let doc = render(MULTI);
        let files = files(&doc);
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["src/main.rs", "src/ui/render.rs", "README.md"]);
        // main.rs: one removed, one added.
        assert_eq!((files[0].added, files[0].removed), (1, 1));
        // render.rs: a new file, two added lines, none removed; its path comes
        // from `+++ b/...` since `--- /dev/null` is the empty side.
        assert_eq!((files[1].added, files[1].removed), (2, 0));
        // README.md: a deletion, path from `--- a/...` since `+++` is /dev/null.
        assert_eq!((files[2].added, files[2].removed), (0, 1));
        // Ranges tile the document without gaps or overlap.
        assert_eq!(files[0].start, 0);
        assert_eq!(files[0].end, files[1].start);
        assert_eq!(files[1].end, files[2].start);
        assert_eq!(files[2].end, doc.rows.len());
        // Each section really is that file's rows.
        assert_eq!(doc.rows[files[1].start].kind, RowKind::FileHeader);
    }

    #[test]
    fn files_is_empty_for_a_patch_without_file_headers() {
        assert!(files(&render("")).is_empty());
        assert!(files(&render("@@ -0,0 +1 @@\n+loose\n")).is_empty());
    }

    #[test]
    fn tree_rows_group_directories_before_files_alphabetically() {
        let doc = render(MULTI);
        let files = files(&doc);
        let tree = tree_rows(&files);
        // Directories (src/) sort before top-level files (README.md); within src/
        // the ui/ subdir sorts before the main.rs leaf.
        let shape: Vec<(usize, &str)> = tree.iter().map(|r| (r.depth, r.name.as_str())).collect();
        assert_eq!(
            shape,
            vec![
                (0, "src"),
                (1, "ui"),
                (2, "render.rs"),
                (1, "main.rs"),
                (0, "README.md"),
            ]
        );
        // The directory nodes carry their full collapse-key path.
        assert!(matches!(&tree[0].kind, TreeKind::Dir { path } if path == "src"));
        assert!(matches!(&tree[1].kind, TreeKind::Dir { path } if path == "src/ui"));
        // The leaves point back at their file index.
        assert!(matches!(tree[2].kind, TreeKind::File { index: 1 }));
        assert!(matches!(tree[3].kind, TreeKind::File { index: 0 }));
        assert!(matches!(tree[4].kind, TreeKind::File { index: 2 }));
    }

    #[test]
    fn tree_rows_keep_an_unresolved_path_as_a_flat_leaf() {
        // A section whose path cannot be resolved (no +++/---, malformed header)
        // still yields a selectable leaf rather than vanishing.
        let doc = render("diff --git weird\nBinary files differ\n");
        let files = files(&doc);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "");
        let tree = tree_rows(&files);
        assert_eq!(tree.len(), 1);
        assert!(matches!(tree[0].kind, TreeKind::File { index: 0 }));
    }

    #[test]
    fn split_rows_slice_offsets_indices_by_the_base() {
        // Folding only the second file's section must reference rows by their
        // position in the whole document, not within the slice.
        let doc = render(MULTI);
        let files = files(&doc);
        let second = &files[1];
        let slice = &doc.rows[second.start..second.end];
        let folded = split_rows_slice(slice, second.start);
        // Every referenced index lands inside the file's own range.
        for row in &folded {
            match *row {
                SplitRow::Full(i) => assert!((second.start..second.end).contains(&i)),
                SplitRow::Pair { left, right } => {
                    for i in [left, right].into_iter().flatten() {
                        assert!((second.start..second.end).contains(&i));
                    }
                }
            }
        }
        // The whole-document helper is the base-0 case.
        assert_eq!(split_rows(&doc), split_rows_slice(&doc.rows, 0));
    }
}

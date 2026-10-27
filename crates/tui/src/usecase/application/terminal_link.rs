//! Pure `http(s)` URL detection and validation over the ANSI-free terminal grid.
//!
//! The daemon streams raw PTY bytes that the shared core
//! [`VtScreen`](usagi_core::usecase::vt_screen::VtScreen) decodes into a
//! character grid; its
//! [`cells`](usagi_core::usecase::vt_screen::VtScreen::cells) and
//! [`cells_with_scrollback`](usagi_core::usecase::vt_screen::VtScreen::cells_with_scrollback)
//! projections hand this module one `String` per row with the ANSI styling and
//! wide-glyph continuation cells already stripped. From that grid this module:
//!
//! - [`url_at`] reads the grid around a clicked cell and, when the cell sits on
//!   an `http(s)` URL, lifts the link out as text (a URL wrapped across rows is
//!   stitched back into one string), so a click anywhere along it opens the
//!   whole link.
//! - [`link_cells`] runs the same detection over the whole grid once, returning
//!   every cell that sits on a URL so the renderer can underline links.
//! - [`validate_url`] is the defense-in-depth gate a browser launcher
//!   runs on a detected string before spawning: it re-checks the scheme
//!   allowlist and rejects any control character, escape, newline, or space, so
//!   an ANSI/terminal-control sequence can never reach a browser argument.
//!
//! This is the pure core of the click-to-open feature. The mouse hit-test and
//! selection-drag coexistence live in presentation; everything here is pure and
//! unit-tested against plain row strings.
//!
//! Because [`cells`](usagi_core::usecase::vt_screen::VtScreen::cells) drops the
//! per-row wrap flag, a logical line is reconstructed from the grid width: a row
//! whose last display column is non-blank is taken to wrap into the next. A line
//! whose real content happens to fill the last column with no trailing space is
//! therefore joined with the row below, the one ambiguity of working from the
//! rendered grid rather than the decoder's wrap bit.

use std::collections::HashSet;
use std::ops::Range;

use unicode_width::UnicodeWidthChar;

use super::terminal_selection::TerminalPoint;

/// The URL schemes detection recognises. Restricted to `http(s)` so an ordinary
/// word (or a bare `host:port`) is never mistaken for a link to open.
const SCHEMES: [&str; 2] = ["https://", "http://"];

/// The terminal grid expanded to one `char` per display column.
///
/// [`cells`](usagi_core::usecase::vt_screen::VtScreen::cells) already dropped
/// wide-glyph continuation cells, so a wide glyph is one `char` spanning two
/// columns. Re-expanding to a column per cell lets a click column index straight
/// into a row and keeps wrapped rows aligned when they are joined.
struct Columns {
    rows: Vec<Vec<char>>,
    width: usize,
}

impl Columns {
    /// Whether `row` wraps into the next one. Without the decoder's wrap bit this
    /// is inferred from the grid: a wrapped row fills its last column, so a
    /// non-blank final column marks the wrap.
    fn wrapped(&self, row: usize) -> bool {
        self.width > 0 && self.rows[row].last().is_some_and(|&ch| ch != ' ')
    }

    /// The last row of the logical line that `row` belongs to (each row but the
    /// last wraps onto the next).
    fn logical_end(&self, row: usize) -> usize {
        let mut end = row;
        while end + 1 < self.rows.len() && self.wrapped(end) {
            end += 1;
        }
        end
    }

    /// The flattened chars of rows `start..=end`, joined column-for-column.
    fn flatten(&self, start: usize, end: usize) -> Vec<char> {
        let mut chars = Vec::with_capacity((end - start + 1) * self.width);
        for row in start..=end {
            chars.extend_from_slice(&self.rows[row]);
        }
        chars
    }

    /// Map a flattened-line index back to its grid cell, given the line's first
    /// row.
    fn cell(&self, start: usize, index: usize) -> TerminalPoint {
        TerminalPoint {
            row: start + index / self.width,
            column: index % self.width,
        }
    }
}

/// The display width of `ch`, at least one column (a zero-width char still
/// occupies the cell it was written to in the decoded grid).
fn char_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0).max(1)
}

/// Expand `viewport` rows into a per-display-column grid. The width is the widest
/// row's display width, which for a real `cells()` projection is the terminal
/// column count every row is padded to.
fn expand(viewport: &[String]) -> Columns {
    let width = viewport
        .iter()
        .map(|row| row.chars().map(char_width).sum())
        .max()
        .unwrap_or(0);
    let rows = viewport.iter().map(|row| columns_of(row, width)).collect();
    Columns { rows, width }
}

/// One `char` per display column for a single row, padded or clipped to `width`.
fn columns_of(row: &str, width: usize) -> Vec<char> {
    let mut cols = Vec::with_capacity(width);
    for ch in row.chars() {
        cols.push(ch);
        // A wide glyph's trailing column(s) carry no character of their own; a
        // blank keeps them out of any URL run by representing the wide-glyph
        // continuation cell explicitly.
        cols.extend(std::iter::repeat_n(' ', char_width(ch) - 1));
    }
    cols.truncate(width);
    cols.resize(width, ' ');
    cols
}

/// Walk out from `row` to the first row of the logical line it belongs to.
fn logical_start(grid: &Columns, row: usize) -> usize {
    let mut start = row;
    while start > 0 && grid.wrapped(start - 1) {
        start -= 1;
    }
    start
}

/// Detect the `http(s)` URL the cell at `point` sits on, returning it as text, or
/// `None` when the cell is blank or not part of a URL. A URL that wrapped onto
/// the next row(s) is stitched back together, so a click anywhere along it yields
/// the whole link.
#[must_use]
pub fn url_at(viewport: &[String], point: TerminalPoint) -> Option<String> {
    let grid = expand(viewport);
    if grid.width == 0 || point.row >= grid.rows.len() || point.column >= grid.width {
        return None;
    }
    let start = logical_start(&grid, point.row);
    let chars = grid.flatten(start, grid.logical_end(point.row));
    let idx = (point.row - start) * grid.width + point.column;
    url_in_chars(&chars, idx)
}

/// Every grid cell that sits on an `http(s)` URL, so the renderer can underline
/// links to mark them clickable. Each logical line is scanned once.
#[must_use]
pub fn link_cells(viewport: &[String]) -> HashSet<TerminalPoint> {
    let grid = expand(viewport);
    let mut cells = HashSet::new();
    if grid.width == 0 {
        return cells;
    }
    let mut start = 0;
    while start < grid.rows.len() {
        let end = grid.logical_end(start);
        let chars = grid.flatten(start, end);
        for span in url_spans(&chars) {
            for index in span {
                cells.insert(grid.cell(start, index));
            }
        }
        start = end + 1;
    }
    cells
}

/// Find the `http(s)` URL covering index `idx` in the flattened line `chars`, or
/// `None` when `idx` is blank or the click lands outside any URL (on the text
/// before a scheme or on trimmed trailing punctuation).
fn url_in_chars(chars: &[char], idx: usize) -> Option<String> {
    if chars[idx].is_whitespace() {
        return None;
    }
    let span = url_spans(chars).into_iter().find(|s| s.contains(&idx))?;
    Some(chars[span].iter().collect())
}

/// Every `http(s)` URL in the flattened line `chars`, as half-open char-index
/// ranges. Each maximal whitespace-free run holds at most one link: the earliest
/// scheme in the run starts it (dropping a leading `(` or stray prefix) and it
/// runs to the first non-URL character (a CJK glyph or full-width punctuation
/// butted against it with no space) with trailing prose punctuation then trimmed.
/// A run whose only scheme has no host is skipped.
fn url_spans(chars: &[char]) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        // A URL never contains a space, so the link is somewhere inside this run.
        let run_start = i;
        let mut run_end = run_start;
        while run_end < chars.len() && !chars[run_end].is_whitespace() {
            run_end += 1;
        }
        if let Some(scheme_off) =
            (run_start..run_end).find(|&j| SCHEMES.iter().any(|s| starts_with_at(chars, j, s)))
        {
            // A URL is ASCII, so it ends at the first character it cannot
            // contain. Japanese text often butts straight against a link with no
            // space (`…/350（補足）`), so without this the run would swallow
            // `（補足）`; stop at that `（` (and any CJK char) here.
            let mut url_end = scheme_off;
            while url_end < run_end && is_url_char(chars[url_end]) {
                url_end += 1;
            }
            let raw: String = chars[scheme_off..url_end].iter().collect();
            let url = trim_trailing(&raw);
            // A bare scheme with no host is not a link.
            if !SCHEMES.contains(&url) {
                spans.push(scheme_off..scheme_off + url.chars().count());
            }
        }
        i = run_end;
    }
    spans
}

/// Whether `c` can appear in a URL. URLs are ASCII, so this is the printable
/// ASCII range (letters, digits, and punctuation — no space, no controls). The
/// first character outside it — a CJK glyph or full-width punctuation such as
/// `（` / `、` / `。` glued to the link with no separating space — ends the URL.
fn is_url_char(c: char) -> bool {
    c.is_ascii_graphic()
}

/// Whether `chars[at..]` begins with `needle`.
fn starts_with_at(chars: &[char], at: usize, needle: &str) -> bool {
    needle
        .chars()
        .enumerate()
        .all(|(off, want)| chars.get(at + off) == Some(&want))
}

/// Trim the trailing punctuation a URL commonly butts up against in prose —
/// sentence marks and quotes always, and a closing bracket only when it is
/// unbalanced (so a Wikipedia-style `..._(disambiguation)` keeps its pair).
fn trim_trailing(url: &str) -> &str {
    let mut url = url;
    while let Some(last) = url.chars().last() {
        let trimmable = match last {
            ')' => count(url, ')') > count(url, '('),
            ']' => count(url, ']') > count(url, '['),
            '}' => count(url, '}') > count(url, '{'),
            '.' | ',' | ';' | ':' | '!' | '?' | '>' | '"' | '\'' | '`' => true,
            _ => false,
        };
        if !trimmable {
            break;
        }
        url = &url[..url.len() - last.len_utf8()];
    }
    url
}

/// How many times `c` appears in `s`.
fn count(s: &str, c: char) -> usize {
    s.chars().filter(|&ch| ch == c).count()
}

/// Why [`validate_url`] refused to open a detected candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlRejection {
    /// The candidate was empty.
    Empty,
    /// The scheme was not `http` or `https` (`javascript:`, `file:`, `data:`,
    /// `mailto:`, or any custom scheme).
    DisallowedScheme,
    /// A control character (`0x00`–`0x1F` / `0x7F`), including `ESC`, a carriage
    /// return, or a newline, was present.
    ControlCharacter,
    /// A whitespace character (a space or other Unicode whitespace) was present.
    Whitespace,
    /// A non-ASCII character was present (a URL to open is ASCII-only).
    NonAscii,
    /// Only the scheme was present, with no host after it.
    MissingHost,
}

/// Validate a detected candidate as an `http(s)` URL safe to hand to a browser,
/// returning the candidate unchanged when it passes.
///
/// Detection already restricts schemes to `http(s)` and characters to printable
/// ASCII, so a candidate from [`url_at`] always passes. This is the
/// defense-in-depth gate a launcher runs immediately before spawning, so an
/// ANSI escape, terminal-control byte, or non-`http(s)` scheme can never reach a
/// browser argument regardless of how the candidate was obtained.
///
/// # Errors
///
/// Returns the [`UrlRejection`] describing the first reason the candidate is not
/// a safe `http(s)` URL: an empty string, a disallowed scheme, an embedded
/// control character, whitespace, a non-ASCII character, or a missing host.
pub fn validate_url(candidate: &str) -> Result<&str, UrlRejection> {
    if candidate.is_empty() {
        return Err(UrlRejection::Empty);
    }
    let Some(scheme) = SCHEMES.iter().find(|s| candidate.starts_with(**s)) else {
        return Err(UrlRejection::DisallowedScheme);
    };
    for ch in candidate.chars() {
        if ch.is_control() {
            return Err(UrlRejection::ControlCharacter);
        }
        if ch.is_whitespace() {
            return Err(UrlRejection::Whitespace);
        }
        if !ch.is_ascii_graphic() {
            return Err(UrlRejection::NonAscii);
        }
    }
    if candidate.len() == scheme.len() {
        return Err(UrlRejection::MissingHost);
    }
    Ok(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A viewport whose rows are padded to `cols` display columns, matching what
    /// [`VtScreen::cells`](usagi_core::usecase::vt_screen::VtScreen::cells)
    /// hands this module. Wrapping is inferred from a full final column, so
    /// padding a short line keeps it a standalone logical line.
    fn grid(cols: usize, lines: &[&str]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                let width: usize = line.chars().map(char_width).sum();
                let mut row = (*line).to_owned();
                row.push_str(&" ".repeat(cols.saturating_sub(width)));
                row
            })
            .collect()
    }

    fn at(row: usize, column: usize) -> TerminalPoint {
        TerminalPoint { row, column }
    }

    fn pairs(cells: HashSet<TerminalPoint>) -> HashSet<(usize, usize)> {
        cells.into_iter().map(|c| (c.row, c.column)).collect()
    }

    #[test]
    fn url_at_lifts_a_link_clicked_anywhere_along_it() {
        let view = grid(40, &["see https://example.com/x now"]);
        // Clicking the scheme, the host, and the path all return the whole URL.
        for col in 4..=24 {
            assert_eq!(
                url_at(&view, at(0, col)).as_deref(),
                Some("https://example.com/x"),
                "col {col}",
            );
        }
    }

    #[test]
    fn url_at_ignores_a_click_on_surrounding_text_or_blanks() {
        let view = grid(40, &["see https://example.com now"]);
        // The leading word, the space before the URL, and the trailing blank
        // padding are not links.
        assert_eq!(url_at(&view, at(0, 0)), None);
        assert_eq!(url_at(&view, at(0, 3)), None);
        assert_eq!(url_at(&view, at(0, 39)), None);
    }

    #[test]
    fn url_at_rejects_a_non_http_scheme() {
        // A bare `host:port` token is not opened.
        let view = grid(20, &["ftp://host:21/file"]);
        assert_eq!(url_at(&view, at(0, 2)), None);
    }

    #[test]
    fn url_at_stitches_a_url_wrapped_across_rows() {
        // The URL fills row 0 (16 cols, non-blank last column) and continues on
        // row 1, so it reads as one logical line.
        let view = grid(16, &["https://example.", "com/page"]);
        assert_eq!(
            url_at(&view, at(0, 0)).as_deref(),
            Some("https://example.com/page"),
        );
        assert_eq!(
            url_at(&view, at(1, 2)).as_deref(),
            Some("https://example.com/page"),
        );
    }

    #[test]
    fn url_at_stitches_a_url_wrapped_across_three_rows() {
        // Two full rows feed a third: `logical_end` must keep walking past the
        // second wrapped row, not stop at the first.
        let view = grid(8, &["https://", "example.", "com/page"]);
        assert_eq!(
            url_at(&view, at(2, 0)).as_deref(),
            Some("https://example.com/page"),
        );
    }

    #[test]
    fn url_at_returns_none_outside_the_grid_or_on_an_empty_viewport() {
        let view = grid(8, &["hi"]);
        assert_eq!(url_at(&view, at(9, 0)), None);
        assert_eq!(url_at(&view, at(0, 8)), None);
        // An empty viewport (zero width) yields no link and does not panic.
        assert_eq!(url_at(&[], at(0, 0)), None);
        assert_eq!(url_at(&[String::new()], at(0, 0)), None);
    }

    #[test]
    fn url_at_skips_a_wide_glyph_and_detects_the_url_after_it() {
        // The full-width "あ" occupies cols 0-1; its trailing column is blank, so
        // a click there finds no link, while the URL after it is detected.
        let view = grid(30, &["あ https://example.com"]);
        assert_eq!(url_at(&view, at(0, 1)), None);
        assert_eq!(
            url_at(&view, at(0, 3)).as_deref(),
            Some("https://example.com"),
        );
    }

    #[test]
    fn url_in_a_parenthesised_run_drops_the_wrapping_parens() {
        // "(https://example.com)" — the leading "(" is skipped to the scheme and
        // the unbalanced trailing ")" is trimmed.
        let view = grid(30, &["(https://example.com)"]);
        assert_eq!(
            url_at(&view, at(0, 5)).as_deref(),
            Some("https://example.com"),
        );
    }

    #[test]
    fn a_bare_scheme_with_no_host_is_not_a_link() {
        let view = grid(12, &["https://"]);
        assert_eq!(url_at(&view, at(0, 0)), None);
    }

    #[test]
    fn clicking_a_prefix_glued_to_a_url_does_not_open_the_prefix() {
        // "see:https://x.io" is one run; the scheme starts mid-run, so a click on
        // the "see:" part lands before the URL and opens nothing.
        let view = grid(20, &["see:https://x.io"]);
        assert_eq!(url_at(&view, at(0, 1)), None);
        assert_eq!(url_at(&view, at(0, 8)).as_deref(), Some("https://x.io"));
    }

    #[test]
    fn full_width_punctuation_glued_to_a_url_is_not_part_of_it() {
        // The link butts straight against a full-width `（…）`; the URL must stop
        // at `（`, not swallow `（補足）`.
        let view = grid(40, &["https://example.com/350（補足）"]);
        assert_eq!(
            url_at(&view, at(0, 5)).as_deref(),
            Some("https://example.com/350"),
        );
        // The full-width `（` (col 23, after the 23-char URL) is not a link.
        assert_eq!(url_at(&view, at(0, 23)), None);
    }

    #[test]
    fn a_cjk_character_glued_to_a_url_ends_it() {
        let view = grid(40, &["https://example.com見て"]);
        assert_eq!(
            url_at(&view, at(0, 5)).as_deref(),
            Some("https://example.com"),
        );
    }

    #[test]
    fn trailing_sentence_punctuation_is_trimmed() {
        assert_eq!(trim_trailing("https://example.com."), "https://example.com");
        assert_eq!(
            trim_trailing("https://example.com),"),
            "https://example.com"
        );
        assert_eq!(
            trim_trailing("https://example.com!?"),
            "https://example.com"
        );
    }

    #[test]
    fn a_balanced_closing_bracket_is_kept_and_an_unbalanced_one_trimmed() {
        // A balanced pair belongs to the path; only an unbalanced bracket is prose.
        assert_eq!(
            trim_trailing("https://en.wikipedia.org/wiki/Foo_(bar)"),
            "https://en.wikipedia.org/wiki/Foo_(bar)",
        );
        assert_eq!(
            trim_trailing("https://example.com/[id]"),
            "https://example.com/[id]",
        );
        assert_eq!(
            trim_trailing("https://example.com/a{b}"),
            "https://example.com/a{b}",
        );
        // Unbalanced closers are prose and are dropped.
        assert_eq!(
            trim_trailing("https://example.com/a)"),
            "https://example.com/a"
        );
        assert_eq!(
            trim_trailing("https://example.com/a]"),
            "https://example.com/a"
        );
        assert_eq!(
            trim_trailing("https://example.com/a}"),
            "https://example.com/a"
        );
    }

    #[test]
    fn link_cells_marks_exactly_the_url_run() {
        let view = grid(40, &["see https://example.com/x now"]);
        // "https://example.com/x" is 21 chars starting at col 4 (after "see "),
        // so cols 4..=24 are link cells; the surrounding words carry none.
        let expected: HashSet<(usize, usize)> = (4..=24).map(|c| (0, c)).collect();
        assert_eq!(pairs(link_cells(&view)), expected);
    }

    #[test]
    fn link_cells_finds_no_link_in_plain_text() {
        let view = grid(20, &["just some words"]);
        assert!(link_cells(&view).is_empty());
    }

    #[test]
    fn link_cells_marks_every_url_across_rows() {
        let view = grid(30, &["https://a.io first", "then https://b.io"]);
        let expected = (0..12)
            .map(|column| (0, column))
            .chain((5..17).map(|column| (1, column)))
            .collect();
        assert_eq!(pairs(link_cells(&view)), expected);
    }

    #[test]
    fn link_cells_spans_a_wrapped_url_across_rows() {
        let view = grid(16, &["https://example.", "com/page"]);
        let cells = pairs(link_cells(&view));
        for col in 0..16 {
            assert!(cells.contains(&(0, col)), "row 0 col {col}");
        }
        for col in 0..8 {
            assert!(cells.contains(&(1, col)), "row 1 col {col}");
        }
        assert!(!cells.contains(&(1, 8)));
    }

    #[test]
    fn link_cells_handles_an_empty_viewport() {
        assert!(link_cells(&[]).is_empty());
        assert!(link_cells(&[String::new()]).is_empty());
    }

    #[test]
    fn validate_url_accepts_http_and_https_with_a_host() {
        assert_eq!(
            validate_url("https://example.com/path?q=1#frag"),
            Ok("https://example.com/path?q=1#frag"),
        );
        assert_eq!(validate_url("http://example.com"), Ok("http://example.com"));
    }

    #[test]
    fn validate_url_rejects_an_empty_candidate() {
        assert_eq!(validate_url(""), Err(UrlRejection::Empty));
    }

    #[test]
    fn validate_url_rejects_disallowed_schemes() {
        for candidate in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "data:text/html,<script>",
            "mailto:a@b.com",
            "ftp://host/file",
            "custom-scheme://x",
            "HTTP://example.com", // exact lowercase only, matching the detector
        ] {
            assert_eq!(
                validate_url(candidate),
                Err(UrlRejection::DisallowedScheme),
                "{candidate}",
            );
        }
    }

    #[test]
    fn validate_url_rejects_control_characters_escape_and_newlines() {
        assert_eq!(
            validate_url("https://ex\u{1b}ample.com"),
            Err(UrlRejection::ControlCharacter),
        );
        assert_eq!(
            validate_url("https://ex\nample.com"),
            Err(UrlRejection::ControlCharacter),
        );
        assert_eq!(
            validate_url("https://ex\u{7f}ample.com"),
            Err(UrlRejection::ControlCharacter),
        );
    }

    #[test]
    fn validate_url_rejects_whitespace() {
        assert_eq!(
            validate_url("https://ex ample.com"),
            Err(UrlRejection::Whitespace),
        );
        // A non-control Unicode space (NBSP) is still whitespace.
        assert_eq!(
            validate_url("https://ex\u{a0}ample.com"),
            Err(UrlRejection::Whitespace),
        );
    }

    #[test]
    fn validate_url_rejects_non_ascii_characters() {
        assert_eq!(validate_url("https://例え.jp"), Err(UrlRejection::NonAscii),);
    }

    #[test]
    fn validate_url_rejects_a_bare_scheme_with_no_host() {
        assert_eq!(validate_url("https://"), Err(UrlRejection::MissingHost));
        assert_eq!(validate_url("http://"), Err(UrlRejection::MissingHost));
    }

    #[test]
    fn url_rejection_derives_are_exercised() {
        let variants = [
            UrlRejection::Empty,
            UrlRejection::DisallowedScheme,
            UrlRejection::ControlCharacter,
            UrlRejection::Whitespace,
            UrlRejection::NonAscii,
            UrlRejection::MissingHost,
        ];
        for variant in variants {
            assert_eq!(variant, variant.clone());
            assert!(!format!("{variant:?}").is_empty());
        }
        assert_ne!(UrlRejection::Empty, UrlRejection::MissingHost);
    }
}

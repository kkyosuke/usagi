//! Turning a click in the embedded terminal pane into a URL to open.
//!
//! When the user clicks (a left press and release with no drag) on a link in the
//! live pane, the pane opens it in the user's default browser — the way a
//! standalone terminal does. This module is the pure core of that feature:
//!
//! - [`url_at`] reads the [`vt100::Screen`] grid around the clicked [`Cell`] and,
//!   if the cell sits on an `http(s)` URL, lifts the link out as text. It stitches
//!   a URL that wrapped across rows back into one string (via
//!   [`vt100::Screen::row_wrapped`]).
//! - [`link_cells`] runs the same detection over the *whole* grid, returning every
//!   cell that sits on a URL so the renderer can underline links as visibly
//!   clickable.
//! - [`link_cells_at`] is the hover counterpart: it returns just the cells of the
//!   one URL under the pointer, so the renderer can recolour the hovered link.
//! - [`open_command`] gives the platform argv that hands a URL to the default
//!   browser (`open` / `xdg-open` / `cmd /c start`).
//!
//! The terminal I/O — translating a mouse report into a [`Cell`] and spawning the
//! browser command — lives in the (coverage-excluded) terminal pane; everything
//! here is pure and unit-tested against a parser driven with bytes.

use std::collections::HashSet;

use super::selection::Cell;
use crate::domain::workspace_state::PrLink;

/// The URL schemes a click recognises. Restricted to `http(s)` so an ordinary
/// word (or a bare `host:port`) is never mistaken for a link to open.
const SCHEMES: [&str; 2] = ["https://", "http://"];

/// Detect the `http(s)` URL the cell at `cell` sits on, returning it as text, or
/// `None` when the cell is blank or not part of a URL. A URL that wrapped onto
/// the next row(s) is stitched back together, so a click anywhere along it opens
/// the whole link.
pub fn url_at(screen: &vt100::Screen, cell: Cell) -> Option<String> {
    let (_, _, chars, idx) = logical_line(screen, cell)?;
    url_in_chars(&chars, idx)
}

/// Every grid cell of the `http(s)` URL that the cell at `cell` sits on, or an
/// empty set when `cell` is blank or not on a URL. This is the hover counterpart
/// to [`link_cells`]: where that marks *every* link on screen (so each reads as
/// underlined), this picks out just the one under the pointer, so the renderer
/// can recolour it and give the hovered link a clickable affordance.
pub fn link_cells_at(screen: &vt100::Screen, cell: Cell) -> HashSet<Cell> {
    let mut cells = HashSet::new();
    let Some((start, width, chars, idx)) = logical_line(screen, cell) else {
        return cells;
    };
    if idx >= chars.len() || chars[idx].is_whitespace() {
        return cells;
    }
    if let Some(span) = url_spans(&chars).into_iter().find(|s| s.contains(&idx)) {
        for i in span {
            let row = start + (i / width) as u16;
            let col = (i % width) as u16;
            cells.insert(Cell::new(row, col));
        }
    }
    cells
}

/// Flatten the logical line (the run of rows wrap-joined by
/// [`vt100::Screen::row_wrapped`]) containing `cell` to one char per column,
/// row-major, returning the line's first row, the grid width, the flattened
/// chars, and `cell`'s index into them — or `None` when `cell` is outside the
/// grid. Shared by [`url_at`] (which reads the URL text) and [`link_cells_at`]
/// (which maps a URL's char span back to its cells).
///
/// A wrapped row has no trailing padding, so the rows join with no gap and a
/// column maps straight to an index. Wide-glyph continuation cells and blanks
/// become spaces — never URL characters — which is all the detection needs.
fn logical_line(screen: &vt100::Screen, cell: Cell) -> Option<(u16, usize, Vec<char>, usize)> {
    let (rows, cols) = screen.size();
    if cell.row >= rows || cell.col >= cols || cols == 0 {
        return None;
    }
    // The clicked row may be the middle of a logical line wrapped across several
    // visible rows; walk out to that line's first and last rows so a wrapped URL
    // reads as one run.
    let mut start = cell.row;
    while start > 0 && screen.row_wrapped(start - 1) {
        start -= 1;
    }
    let mut end = cell.row;
    while end + 1 < rows && screen.row_wrapped(end) {
        end += 1;
    }
    let width = cols as usize;
    let mut chars: Vec<char> = Vec::with_capacity((end - start + 1) as usize * width);
    for row in start..=end {
        for col in 0..cols {
            chars.push(cell_char(screen.cell(row, col)));
        }
    }
    let idx = (cell.row - start) as usize * width + cell.col as usize;
    Some((start, width, chars, idx))
}

/// The single representative character of a grid cell: its first glyph, or a
/// space for a blank cell or the trailing half of a wide glyph (whose content is
/// already on the lead cell). Keeping it one-char-per-column lets a click column
/// index straight into the flattened line.
fn cell_char(cell: Option<&vt100::Cell>) -> char {
    match cell {
        Some(cell) if cell.has_contents() => cell.contents().chars().next().unwrap_or(' '),
        _ => ' ',
    }
}

/// Find the `http(s)` URL covering index `idx` in the flattened line `chars`, or
/// `None` when `idx` is blank, outside any run, or in a run that holds no URL the
/// click actually lands on.
fn url_in_chars(chars: &[char], idx: usize) -> Option<String> {
    if idx >= chars.len() || chars[idx].is_whitespace() {
        return None;
    }
    // The click must land on the URL itself, not on the text before its scheme or
    // the trailing punctuation trimmed off its tail.
    let span = url_spans(chars).into_iter().find(|s| s.contains(&idx))?;
    Some(chars[span].iter().collect())
}

/// Every `http(s)` URL in the flattened line `chars`, as half-open char-index
/// ranges. Each maximal whitespace-free run holds at most one link: the earliest
/// scheme in the run starts it (dropping a leading `(` or stray prefix) and it
/// runs to the first non-URL character (a CJK glyph or full-width punctuation
/// butted against it with no space, see [`is_url_char`]) with trailing prose
/// punctuation then trimmed (see [`trim_trailing`]). A run whose only scheme has
/// no host is skipped.
fn url_spans(chars: &[char]) -> Vec<std::ops::Range<usize>> {
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
            // A URL is ASCII, so it ends at the first character it cannot contain.
            // Japanese text often butts straight against a link with no space
            // (`…/350（補足）`), so without this the run would swallow `（補足）`
            // into the link; stop at that `（` (and any CJK char) here.
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

/// Every grid cell that sits on an `http(s)` URL, so the renderer can underline
/// links to mark them clickable. This is [`url_at`]'s detection
/// run over the whole screen at once: each logical line (a run of rows stitched
/// by [`vt100::Screen::row_wrapped`]) is flattened the same way, its URL spans are
/// found, and each covered index maps back to its `(row, col)` cell.
pub fn link_cells(screen: &vt100::Screen) -> HashSet<Cell> {
    let (rows, cols) = screen.size();
    let mut cells = HashSet::new();
    if cols == 0 {
        return cells;
    }
    let width = cols as usize;
    // One scratch buffer reused across logical lines: this scan runs while the
    // render loop holds the parser lock, so allocating a fresh `Vec<char>` per
    // logical line would add avoidable churn to that critical section.
    let mut chars: Vec<char> = Vec::with_capacity(width);
    let mut start = 0;
    while start < rows {
        // Extend to the last row of this logical line (each row but the last
        // wraps onto the next), so a wrapped URL is detected as one run.
        let mut end = start;
        while end + 1 < rows && screen.row_wrapped(end) {
            end += 1;
        }
        chars.clear();
        for row in start..=end {
            for col in 0..cols {
                chars.push(cell_char(screen.cell(row, col)));
            }
        }
        for span in url_spans(&chars) {
            for idx in span {
                let row = start + (idx / width) as u16;
                let col = (idx % width) as u16;
                cells.insert(Cell::new(row, col));
            }
        }
        start = end + 1;
    }
    cells
}

/// Every distinct pull-request link visible on `screen`, in reading order
/// (top-to-bottom), with duplicate URLs dropped.
///
/// This is how usagi learns a session's PRs without querying GitHub: the embedded
/// agent prints PR URLs in its replies (e.g. after opening one), and those URLs are
/// already detected as clickable links here. We reuse that same whole-screen scan
/// ([`screen_urls`]) and keep the URLs that look like a pull request
/// ([`parse_pr_url`]). A session may touch several repositories and open a PR in
/// each, so all of them are returned (de-duplicated by URL); the caller records
/// them against the session so the sidebar can show the `#<number>` badges and a
/// click can reopen them.
pub fn pr_links(screen: &vt100::Screen) -> Vec<PrLink> {
    let mut out: Vec<PrLink> = Vec::new();
    for pr in screen_urls(screen).iter().filter_map(|u| parse_pr_url(u)) {
        if !out.iter().any(|p| p.url == pr.url) {
            out.push(pr);
        }
    }
    out
}

/// Parse a `http(s)` URL into a [`PrLink`] when it is a pull-request URL of the
/// form `https://<host>/<owner>/<repo>/pull/<N>` (GitHub and GitHub Enterprise),
/// or `None` otherwise. The number is the `<N>` path segment; a path with no
/// owner/repo before `pull`, or a non-numeric / overflowing `<N>`, is rejected.
/// Trailing path segments (`/pull/<N>/files`) are tolerated — only the segment
/// right after `pull` is read.
pub fn parse_pr_url(url: &str) -> Option<PrLink> {
    let rest = SCHEMES.iter().find_map(|s| url.strip_prefix(s))?;
    let segments: Vec<&str> = rest.split('/').collect();
    // host / owner / repo / "pull" / <N>: `pull` sits at index 3 at the earliest,
    // so there is always an owner and repo (and a host) ahead of it.
    let pull = segments.iter().position(|&s| s == "pull")?;
    if pull < 3 {
        return None;
    }
    let number = segments.get(pull + 1)?;
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(PrLink {
        number: number.parse().ok()?,
        url: url.to_string(),
    })
}

/// Every `http(s)` URL on `screen`, as text, in reading order (top-to-bottom,
/// left-to-right). The string counterpart to [`link_cells`]: it flattens each
/// logical line the same way and lifts each URL span as a string, rather than
/// marking the cells it covers. Used by [`pr_link`] to find a pull-request URL in
/// the agent's output.
fn screen_urls(screen: &vt100::Screen) -> Vec<String> {
    let (rows, cols) = screen.size();
    let mut urls = Vec::new();
    if cols == 0 {
        return urls;
    }
    let width = cols as usize;
    let mut chars: Vec<char> = Vec::with_capacity(width);
    let mut start = 0;
    while start < rows {
        let mut end = start;
        while end + 1 < rows && screen.row_wrapped(end) {
            end += 1;
        }
        chars.clear();
        for row in start..=end {
            for col in 0..cols {
                chars.push(cell_char(screen.cell(row, col)));
            }
        }
        for span in url_spans(&chars) {
            urls.push(chars[span].iter().collect());
        }
        start = end + 1;
    }
    urls
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

/// The argv that opens `url` in the user's default browser on this platform:
/// macOS `open`, Linux/BSD `xdg-open`, Windows `cmd /c start`. Empty when the
/// platform is unrecognised, so the caller spawns nothing.
pub fn open_command(url: &str) -> Vec<String> {
    #[cfg(target_os = "macos")]
    {
        vec!["open".to_string(), url.to_string()]
    }
    #[cfg(target_os = "windows")]
    {
        // The empty `""` is `start`'s title argument; without it a quoted URL
        // would be taken as the window title instead of the target.
        vec![
            "cmd".to_string(),
            "/C".to_string(),
            "start".to_string(),
            String::new(),
            url.to_string(),
        ]
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        vec!["xdg-open".to_string(), url.to_string()]
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", unix)))]
    {
        let _ = url;
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A parser sized `rows`×`cols`, fed `bytes`, returned for inspection.
    fn parsed(rows: u16, cols: u16, bytes: &[u8]) -> vt100::Parser {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(bytes);
        parser
    }

    #[test]
    fn url_at_lifts_a_link_clicked_anywhere_along_it() {
        let parser = parsed(1, 40, b"see https://example.com/x now");
        let screen = parser.screen();
        // Clicking the scheme, the host, and the path all return the whole URL.
        for col in 4..=22 {
            assert_eq!(
                url_at(screen, Cell::new(0, col)).as_deref(),
                Some("https://example.com/x"),
                "col {col}",
            );
        }
    }

    #[test]
    fn url_at_ignores_a_click_on_surrounding_text_or_blanks() {
        let parser = parsed(1, 40, b"see https://example.com now");
        let screen = parser.screen();
        // The leading word, the space before the URL, and the trailing blank
        // padding are not links.
        assert_eq!(url_at(screen, Cell::new(0, 0)), None);
        assert_eq!(url_at(screen, Cell::new(0, 3)), None);
        assert_eq!(url_at(screen, Cell::new(0, 39)), None);
    }

    #[test]
    fn url_at_rejects_a_non_http_scheme() {
        // A bare `host:port` token is not opened.
        let parser = parsed(1, 20, b"ftp://host:21/file");
        assert_eq!(url_at(parser.screen(), Cell::new(0, 2)), None);
    }

    #[test]
    fn url_at_stitches_a_url_wrapped_across_rows() {
        // The URL fills row 0 and continues on row 1; vt100 marks row 0 wrapped.
        let parser = parsed(2, 16, b"https://example.com/page");
        let screen = parser.screen();
        assert!(screen.row_wrapped(0));
        // A click on the first row and on the wrapped tail both yield the join.
        assert_eq!(
            url_at(screen, Cell::new(0, 0)).as_deref(),
            Some("https://example.com/page"),
        );
        assert_eq!(
            url_at(screen, Cell::new(1, 2)).as_deref(),
            Some("https://example.com/page"),
        );
    }

    #[test]
    fn url_at_returns_none_outside_the_grid() {
        let parser = parsed(2, 8, b"hi");
        let screen = parser.screen();
        assert_eq!(url_at(screen, Cell::new(9, 0)), None);
        assert_eq!(url_at(screen, Cell::new(0, 9)), None);
    }

    #[test]
    fn url_at_skips_a_wide_glyph_continuation_cell() {
        // The full-width "あ" occupies cols 0-1; its continuation cell is blank,
        // so a click there finds no link.
        let parser = parsed(1, 30, "あ https://example.com".as_bytes());
        let screen = parser.screen();
        assert_eq!(url_at(screen, Cell::new(0, 1)), None);
        // The URL after the wide glyph is still detected.
        assert_eq!(
            url_at(screen, Cell::new(0, 5)).as_deref(),
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
    fn a_balanced_closing_bracket_is_kept() {
        // The pair belongs to the path; only an unbalanced bracket is prose.
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
    }

    #[test]
    fn url_in_a_parenthesised_run_drops_the_wrapping_parens() {
        // "(https://example.com)" — the leading "(" is skipped to the scheme and
        // the unbalanced trailing ")" is trimmed.
        let parser = parsed(1, 30, b"(https://example.com)");
        assert_eq!(
            url_at(parser.screen(), Cell::new(0, 5)).as_deref(),
            Some("https://example.com"),
        );
    }

    #[test]
    fn a_bare_scheme_with_no_host_is_not_a_link() {
        let parser = parsed(1, 12, b"https://");
        assert_eq!(url_at(parser.screen(), Cell::new(0, 0)), None);
    }

    #[test]
    fn clicking_a_prefix_glued_to_a_url_does_not_open_the_prefix() {
        // "see:https://x" is one run; the scheme starts mid-run, so a click on
        // the "see:" part lands before the URL and opens nothing, while a click
        // on the URL part opens it.
        let parser = parsed(1, 20, b"see:https://x.io");
        let screen = parser.screen();
        assert_eq!(url_at(screen, Cell::new(0, 1)), None);
        assert_eq!(
            url_at(screen, Cell::new(0, 8)).as_deref(),
            Some("https://x.io"),
        );
    }

    #[test]
    fn full_width_punctuation_glued_to_a_url_is_not_part_of_it() {
        // Japanese prose often has no space before a parenthetical, so the link
        // butts straight against a full-width `（…）`. The URL must stop at `（`,
        // not swallow `（補足）` — the original bug this guards against.
        let parser = parsed(1, 40, "https://example.com/350（補足）".as_bytes());
        let screen = parser.screen();
        // A click inside the URL returns just the URL.
        assert_eq!(
            url_at(screen, Cell::new(0, 5)).as_deref(),
            Some("https://example.com/350"),
        );
        // The full-width `（` (col 23, after the 23-char URL) is not a link.
        assert_eq!(url_at(screen, Cell::new(0, 23)), None);
    }

    #[test]
    fn a_cjk_character_glued_to_a_url_ends_it() {
        // No space between the link and the following Japanese text: the URL ends
        // at the first CJK character rather than absorbing "見て".
        let parser = parsed(1, 40, "https://example.com見て".as_bytes());
        assert_eq!(
            url_at(parser.screen(), Cell::new(0, 5)).as_deref(),
            Some("https://example.com"),
        );
    }

    #[test]
    fn link_cells_stop_at_full_width_punctuation() {
        // The underline run covers only the URL, not the glued `（…）`.
        let parser = parsed(1, 40, "https://example.com（x）".as_bytes());
        let cells = link_pairs(parser.screen());
        // "https://example.com" is 19 chars at cols 0..=18; the full-width `（`
        // begins at col 19 and carries no link cell.
        assert!(cells.contains(&(0, 18))); // "m" of .com
        assert!(!cells.contains(&(0, 19))); // full-width "（"
    }

    /// The set of `(row, col)` pairs `link_cells` marks for `screen`.
    fn link_pairs(screen: &vt100::Screen) -> std::collections::HashSet<(u16, u16)> {
        link_cells(screen)
            .into_iter()
            .map(|c| (c.row, c.col))
            .collect()
    }

    #[test]
    fn link_cells_marks_exactly_the_url_run() {
        let parser = parsed(1, 40, b"see https://example.com/x now");
        let cells = link_pairs(parser.screen());
        // "https://example.com/x" is 21 chars starting at col 4 (after "see "),
        // so cols 4..=24 are link cells; the surrounding words and trailing
        // blanks carry none.
        let expected: std::collections::HashSet<(u16, u16)> = (4..=24).map(|c| (0, c)).collect();
        assert_eq!(cells, expected);
    }

    #[test]
    fn link_cells_finds_no_link_in_plain_text() {
        let parser = parsed(1, 20, b"just some words");
        assert!(link_cells(parser.screen()).is_empty());
    }

    #[test]
    fn link_cells_spans_a_wrapped_url_across_rows() {
        // The URL fills row 0 and continues on row 1 (row 0 is marked wrapped),
        // so cells on both rows are marked up to the URL's end.
        let parser = parsed(2, 16, b"https://example.com/page");
        let screen = parser.screen();
        assert!(screen.row_wrapped(0));
        let cells = link_pairs(screen);
        // Row 0 is filled (16 cells); row 1 holds the 8-char tail.
        for col in 0..16 {
            assert!(cells.contains(&(0, col)), "row 0 col {col}");
        }
        for col in 0..8 {
            assert!(cells.contains(&(1, col)), "row 1 col {col}");
        }
        assert!(!cells.contains(&(1, 8)));
    }

    #[test]
    fn link_cells_trims_trailing_punctuation_and_skips_the_prefix() {
        // "(https://example.com)" — the wrapping parens are not part of the link.
        let parser = parsed(1, 30, b"(https://example.com).");
        let cells = link_pairs(parser.screen());
        assert!(!cells.contains(&(0, 0))); // leading "("
        assert!(cells.contains(&(0, 1))); // "h" of https
        assert!(cells.contains(&(0, 19))); // "m" of .com
        assert!(!cells.contains(&(0, 20))); // trailing ")"
        assert!(!cells.contains(&(0, 21))); // trailing "."
    }

    #[test]
    fn link_cells_at_returns_the_hovered_urls_cells() {
        // Hovering anywhere on the URL yields exactly that URL's cells — the same
        // run `link_cells` marks, but only for the link under the pointer.
        let parser = parsed(1, 40, b"see https://example.com/x now");
        let screen = parser.screen();
        let expected: std::collections::HashSet<(u16, u16)> = (4..=24).map(|c| (0, c)).collect();
        for col in 4..=24 {
            let cells: std::collections::HashSet<(u16, u16)> =
                link_cells_at(screen, Cell::new(0, col))
                    .into_iter()
                    .map(|c| (c.row, c.col))
                    .collect();
            assert_eq!(cells, expected, "hover col {col}");
        }
    }

    #[test]
    fn link_cells_at_picks_only_the_hovered_link_among_several() {
        // Two URLs on one row: hovering the first highlights only the first.
        let parser = parsed(1, 60, b"https://a.io and https://b.io");
        let cells = link_pairs_at(parser.screen(), Cell::new(0, 0));
        // "https://a.io" is 12 chars at cols 0..=11; the second URL is untouched.
        let expected: std::collections::HashSet<(u16, u16)> = (0..=11).map(|c| (0, c)).collect();
        assert_eq!(cells, expected);
    }

    #[test]
    fn link_cells_at_spans_a_wrapped_url() {
        // A URL wrapped across two rows: hovering the tail still yields every cell.
        let parser = parsed(2, 16, b"https://example.com/page");
        let cells = link_pairs_at(parser.screen(), Cell::new(1, 2));
        for col in 0..16 {
            assert!(cells.contains(&(0, col)), "row 0 col {col}");
        }
        for col in 0..8 {
            assert!(cells.contains(&(1, col)), "row 1 col {col}");
        }
    }

    #[test]
    fn link_cells_at_returns_empty_off_a_link() {
        let parser = parsed(1, 40, b"see https://example.com now");
        let screen = parser.screen();
        // A blank cell, the surrounding text, and a cell outside the grid: no link.
        assert!(link_cells_at(screen, Cell::new(0, 0)).is_empty());
        assert!(link_cells_at(screen, Cell::new(0, 3)).is_empty());
        assert!(link_cells_at(screen, Cell::new(0, 39)).is_empty());
        assert!(link_cells_at(screen, Cell::new(9, 0)).is_empty());
    }

    /// The set of `(row, col)` pairs `link_cells_at` marks for a hover at `cell`.
    fn link_pairs_at(screen: &vt100::Screen, cell: Cell) -> std::collections::HashSet<(u16, u16)> {
        link_cells_at(screen, cell)
            .into_iter()
            .map(|c| (c.row, c.col))
            .collect()
    }

    #[test]
    fn link_cells_handles_a_zero_width_screen() {
        // A degenerate 0-column screen yields no link cells (and does not panic).
        let parser = parsed(1, 0, b"");
        assert!(link_cells(parser.screen()).is_empty());
    }

    #[test]
    fn parse_pr_url_reads_the_number_from_a_pull_request_url() {
        let pr = parse_pr_url("https://github.com/KKyosuke/usagi/pull/412").unwrap();
        assert_eq!(pr.number, 412);
        assert_eq!(pr.url, "https://github.com/KKyosuke/usagi/pull/412");
        // A trailing path segment after the number is tolerated.
        assert_eq!(
            parse_pr_url("https://github.com/o/r/pull/7/files")
                .unwrap()
                .number,
            7,
        );
        // GitHub Enterprise (any host) works the same.
        assert_eq!(
            parse_pr_url("https://ghe.corp.example/team/app/pull/99")
                .unwrap()
                .number,
            99,
        );
    }

    #[test]
    fn parse_pr_url_rejects_non_pull_request_urls() {
        // Not a pull-request path.
        assert!(parse_pr_url("https://github.com/KKyosuke/usagi").is_none());
        assert!(parse_pr_url("https://github.com/KKyosuke/usagi/issues/412").is_none());
        // `pull` with no owner/repo ahead of it.
        assert!(parse_pr_url("https://github.com/pull/1").is_none());
        // A non-numeric or missing number.
        assert!(parse_pr_url("https://github.com/o/r/pull/abc").is_none());
        assert!(parse_pr_url("https://github.com/o/r/pull/").is_none());
        // A non-http(s) scheme is not a link we open.
        assert!(parse_pr_url("ftp://github.com/o/r/pull/1").is_none());
        // A number too large for u32 overflows and is rejected.
        assert!(parse_pr_url("https://github.com/o/r/pull/99999999999").is_none());
    }

    #[test]
    fn pr_links_finds_the_pull_request_url_on_screen() {
        let parser = parsed(
            1,
            60,
            b"opened PR: https://github.com/KKyosuke/usagi/pull/412 done",
        );
        let prs = pr_links(parser.screen());
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].number, 412);
        assert_eq!(prs[0].url, "https://github.com/KKyosuke/usagi/pull/412");
    }

    #[test]
    fn pr_links_collects_every_pr_in_reading_order_and_dedups_urls() {
        // Three PR URLs across rows, the first repeated: returned in reading order
        // (top-to-bottom) with the duplicate dropped.
        let parser = parsed(
            3,
            40,
            b"https://github.com/o/r/pull/1\r\nhttps://github.com/o/s/pull/2\r\nhttps://github.com/o/r/pull/1",
        );
        let numbers: Vec<u32> = pr_links(parser.screen()).iter().map(|p| p.number).collect();
        assert_eq!(numbers, vec![1, 2]);
    }

    #[test]
    fn pr_links_finds_a_pull_request_url_wrapped_across_rows() {
        // The URL fills row 0 and continues on row 1; vt100 marks row 0 wrapped, so
        // `screen_urls` stitches the two rows back into one link before parsing.
        let parser = parsed(2, 20, b"https://github.com/o/r/pull/42");
        assert!(parser.screen().row_wrapped(0));
        let prs = pr_links(parser.screen());
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].number, 42);
        assert_eq!(prs[0].url, "https://github.com/o/r/pull/42");
    }

    #[test]
    fn pr_links_is_empty_without_a_pull_request_url() {
        // A plain (non-PR) link on screen, and a zero-width screen, both yield none.
        let parser = parsed(1, 40, b"see https://example.com/x now");
        assert!(pr_links(parser.screen()).is_empty());
        let empty = parsed(1, 0, b"");
        assert!(pr_links(empty.screen()).is_empty());
    }

    #[test]
    fn open_command_targets_the_url_on_this_platform() {
        let argv = open_command("https://example.com");
        assert!(argv.iter().any(|a| a == "https://example.com"));
        // The first element is the program to spawn.
        assert!(!argv.is_empty());
        #[cfg(target_os = "macos")]
        assert_eq!(argv, ["open", "https://example.com"]);
        #[cfg(all(unix, not(target_os = "macos")))]
        assert_eq!(argv, ["xdg-open", "https://example.com"]);
    }
}

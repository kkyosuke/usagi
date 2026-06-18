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
//! - [`open_command`] gives the platform argv that hands a URL to the default
//!   browser (`open` / `xdg-open` / `cmd /c start`).
//!
//! The terminal I/O — translating a mouse report into a [`Cell`] and spawning the
//! browser command — lives in the (coverage-excluded) terminal pane; everything
//! here is pure and unit-tested against a parser driven with bytes.

use super::terminal_selection::Cell;

/// The URL schemes a click recognises. Restricted to `http(s)` so an ordinary
/// word (or a bare `host:port`) is never mistaken for a link to open.
const SCHEMES: [&str; 2] = ["https://", "http://"];

/// Detect the `http(s)` URL the cell at `cell` sits on, returning it as text, or
/// `None` when the cell is blank or not part of a URL. A URL that wrapped onto
/// the next row(s) is stitched back together, so a click anywhere along it opens
/// the whole link.
pub fn url_at(screen: &vt100::Screen, cell: Cell) -> Option<String> {
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
    // Flatten the logical line to one char per column, row-major, so a column
    // maps straight to an index (a wrapped row has no trailing padding, so the
    // rows join with no gap). Wide-glyph continuation cells and blanks become
    // spaces — never URL characters — which is all the detection needs.
    let mut chars: Vec<char> = Vec::with_capacity((end - start + 1) as usize * cols as usize);
    for row in start..=end {
        for col in 0..cols {
            chars.push(cell_char(screen.cell(row, col)));
        }
    }
    let idx = (cell.row - start) as usize * cols as usize + cell.col as usize;
    url_in_chars(&chars, idx)
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
    // The maximal whitespace-free run around the click — a URL never contains a
    // space, so the link is somewhere inside this run.
    let mut run_start = idx;
    while run_start > 0 && !chars[run_start - 1].is_whitespace() {
        run_start -= 1;
    }
    let mut run_end = idx + 1;
    while run_end < chars.len() && !chars[run_end].is_whitespace() {
        run_end += 1;
    }
    // The earliest scheme in the run starts the link (so a leading `(` or stray
    // prefix before `https://` is dropped).
    let scheme_off =
        (run_start..run_end).find(|&i| SCHEMES.iter().any(|s| starts_with_at(chars, i, s)))?;
    let raw: String = chars[scheme_off..run_end].iter().collect();
    let url = trim_trailing(&raw);
    // A bare scheme with no host is not a link.
    if SCHEMES.contains(&url) {
        return None;
    }
    // The click must land on the URL itself, not on text before the scheme.
    let url_end = scheme_off + url.chars().count();
    if idx < scheme_off || idx >= url_end {
        return None;
    }
    Some(url.to_string())
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

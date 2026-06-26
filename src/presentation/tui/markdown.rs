//! A small Markdown renderer for the workspace screen's right-pane preview.
//!
//! This is a deliberately tiny subset of Markdown — enough to make a `README.md`
//! or a design note readable in the terminal, not a conformant CommonMark
//! parser. [`render`] turns the source text into a list of [`MarkdownLine`]s,
//! each a block kind (heading / list item / quote / code / plain) plus a run of
//! inline [`Span`]s carrying their own emphasis. The result is **pure data**: no
//! terminal escapes are produced here, so the parsing is directly testable.
//! Turning a [`MarkdownLine`] into a styled terminal row is the UI layer's job
//! (see the home screen's `panes` module).
//!
//! Supported: ATX headings (`#`…`######`), unordered (`-`/`*`/`+`) and ordered
//! (`1.`/`1)`) lists, block quotes (`>`), fenced code blocks (``` ``` ``` / `~~~`),
//! and the inline spans `**strong**` / `__strong__`, `*em*` / `_em_`,
//! `` `code` ``, and `[link text](url)` (the URL is dropped, the text kept).
//!
//! Fenced code blocks are syntax-highlighted by their info string (the language
//! token after the opening fence) via the [`highlight`] module: each line
//! becomes several [`SpanStyle::Code`] spans carrying a per-token foreground
//! [`Rgb`]. An unknown or absent language falls back to plain, uncoloured text.

mod highlight;

/// The inline emphasis of a run of text within a rendered line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanStyle {
    /// No emphasis.
    Plain,
    /// Bold (`**…**` / `__…__`).
    Strong,
    /// Italic (`*…*` / `_…_`).
    Emphasis,
    /// Inline code (`` `…` ``).
    Code,
    /// The visible text of a link (`[text](url)`); the URL is not shown.
    Link,
}

/// A 24-bit foreground colour for a syntax-highlighted code span. Carried as
/// plain data; the UI layer maps it to a terminal colour when drawing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// A run of text with a single inline style, and an optional foreground colour
/// set only for syntax-highlighted code spans (otherwise the UI colours the run
/// by its [`SpanStyle`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: SpanStyle,
    pub color: Option<Rgb>,
}

impl Span {
    fn new(text: impl Into<String>, style: SpanStyle) -> Self {
        Self {
            text: text.into(),
            style,
            color: None,
        }
    }

    /// A span carrying an explicit foreground colour (used for highlighted code).
    fn colored(text: impl Into<String>, style: SpanStyle, color: Rgb) -> Self {
        Self {
            text: text.into(),
            style,
            color: Some(color),
        }
    }
}

/// The block kind of a rendered line, which governs its prefix and base colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineStyle {
    /// A plain paragraph line (or a blank line, with no spans).
    Text,
    /// An ATX heading of the given level (1–6).
    Heading(u8),
    /// An unordered list item.
    Bullet,
    /// An ordered list item.
    Number,
    /// A block-quote line.
    Quote,
    /// A line inside a fenced code block (rendered verbatim, no inline parsing).
    Code,
}

/// One rendered Markdown line: its block kind, a leading marker (`• `, `1. `, the
/// quote bar `│ `, or empty), and the inline spans of its content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownLine {
    pub style: LineStyle,
    pub prefix: String,
    pub spans: Vec<Span>,
}

impl MarkdownLine {
    /// The line's text with all inline styling dropped — its spans concatenated,
    /// without the prefix. Handy for tests and width measurement.
    pub fn plain_text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }
}

/// Render `source` Markdown into a list of styled lines, one per source line.
pub fn render(source: &str) -> Vec<MarkdownLine> {
    // Empty input renders nothing — `split('\n')` would otherwise yield one
    // spurious blank line for the empty string.
    if source.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut in_code_block = false;
    // Body lines of the current fenced block and its language token, buffered so
    // the whole block can be syntax-highlighted at once (multi-line state needs
    // the lines together).
    let mut code_lines: Vec<&str> = Vec::new();
    let mut code_lang = String::new();
    for raw in source.split('\n') {
        // Tolerate CRLF input by dropping a trailing carriage return.
        let line = raw.strip_suffix('\r').unwrap_or(raw);

        // A fence toggles the code block; the fence delimiter line is not emitted.
        if is_fence(line) {
            if in_code_block {
                flush_code_block(&mut out, &code_lines, &code_lang);
                code_lines.clear();
                code_lang.clear();
            } else {
                code_lang = fence_lang(line);
            }
            in_code_block = !in_code_block;
            continue;
        }
        if in_code_block {
            code_lines.push(line);
            continue;
        }
        out.push(render_block(line));
    }
    // An unterminated fence at end of input still renders its buffered body.
    if in_code_block {
        flush_code_block(&mut out, &code_lines, &code_lang);
    }
    out
}

/// Syntax-highlight `code_lines` (written in `lang`) and append one
/// [`LineStyle::Code`] line per source line to `out`.
fn flush_code_block(out: &mut Vec<MarkdownLine>, code_lines: &[&str], lang: &str) {
    for spans in highlight::highlight_block(code_lines, lang) {
        out.push(MarkdownLine {
            style: LineStyle::Code,
            prefix: String::new(),
            spans,
        });
    }
}

/// Whether `line` is a code-fence delimiter (``` ``` ``` or `~~~`, with optional
/// leading spaces and an optional info string).
fn is_fence(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

/// The language token of an opening fence: the first whitespace-delimited word
/// of its info string, lowercased (e.g. `` ```rust `` → `"rust"`). Empty when the
/// fence has no info string.
fn fence_lang(line: &str) -> String {
    let trimmed = line.trim_start();
    trimmed
        .trim_start_matches(['`', '~'])
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_lowercase()
}

/// Classify a single non-code line into its block kind and inline spans.
fn render_block(line: &str) -> MarkdownLine {
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();

    if trimmed.is_empty() {
        return MarkdownLine {
            style: LineStyle::Text,
            prefix: String::new(),
            spans: Vec::new(),
        };
    }

    if let Some((level, content)) = parse_heading(trimmed) {
        return MarkdownLine {
            style: LineStyle::Heading(level),
            prefix: String::new(),
            spans: parse_inline(content),
        };
    }

    if let Some(content) = parse_quote(trimmed) {
        return MarkdownLine {
            style: LineStyle::Quote,
            prefix: "│ ".to_string(),
            spans: parse_inline(content),
        };
    }

    if let Some(content) = parse_bullet(trimmed) {
        return MarkdownLine {
            style: LineStyle::Bullet,
            prefix: format!("{}• ", " ".repeat(indent)),
            spans: parse_inline(content),
        };
    }

    if let Some((number, content)) = parse_ordered(trimmed) {
        return MarkdownLine {
            style: LineStyle::Number,
            prefix: format!("{}{number}. ", " ".repeat(indent)),
            spans: parse_inline(content),
        };
    }

    MarkdownLine {
        style: LineStyle::Text,
        prefix: String::new(),
        spans: parse_inline(line),
    }
}

/// Parse an ATX heading: 1–6 leading `#`s followed by a space or end-of-line.
/// Returns the level and the trimmed heading text.
fn parse_heading(trimmed: &str) -> Option<(u8, &str)> {
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &trimmed[hashes..];
    if rest.is_empty() {
        return Some((hashes as u8, ""));
    }
    if rest.starts_with(' ') {
        return Some((hashes as u8, rest.trim_start()));
    }
    None
}

/// Parse a block quote: a leading `>` and an optional following space.
fn parse_quote(trimmed: &str) -> Option<&str> {
    let rest = trimmed.strip_prefix('>')?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

/// Parse an unordered list item marker (`- `, `* `, or `+ `), returning the
/// content after it.
fn parse_bullet(trimmed: &str) -> Option<&str> {
    for marker in ["- ", "* ", "+ "] {
        if let Some(content) = trimmed.strip_prefix(marker) {
            return Some(content);
        }
    }
    None
}

/// Parse an ordered list item marker (`<digits>. ` or `<digits>) `), returning
/// the number text and the content after the marker.
fn parse_ordered(trimmed: &str) -> Option<(&str, &str)> {
    let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let after = &trimmed[digits..];
    let content = after
        .strip_prefix(". ")
        .or_else(|| after.strip_prefix(") "))?;
    Some((&trimmed[..digits], content))
}

/// Parse inline emphasis in `text` into a run of styled spans. Unterminated
/// markers are treated as literal text, so partial syntax never swallows the
/// rest of the line.
fn parse_inline(text: &str) -> Vec<Span> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        match c {
            '`' => {
                if let Some(close) = find_char(&chars, i + 1, '`') {
                    flush(&mut buf, &mut spans);
                    spans.push(Span::new(collect(&chars, i + 1, close), SpanStyle::Code));
                    i = close + 1;
                    continue;
                }
            }
            '[' => {
                if let Some((label, end)) = parse_link(&chars, i) {
                    flush(&mut buf, &mut spans);
                    spans.push(Span::new(label, SpanStyle::Link));
                    i = end;
                    continue;
                }
            }
            '*' | '_' => {
                // Strong (doubled marker) is tried before emphasis (single).
                if chars.get(i + 1) == Some(&c) {
                    if let Some(close) = find_double(&chars, i + 2, c) {
                        flush(&mut buf, &mut spans);
                        spans.push(Span::new(collect(&chars, i + 2, close), SpanStyle::Strong));
                        i = close + 2;
                        continue;
                    }
                } else if let Some(close) = find_char(&chars, i + 1, c) {
                    flush(&mut buf, &mut spans);
                    spans.push(Span::new(
                        collect(&chars, i + 1, close),
                        SpanStyle::Emphasis,
                    ));
                    i = close + 1;
                    continue;
                }
            }
            _ => {}
        }
        buf.push(c);
        i += 1;
    }

    flush(&mut buf, &mut spans);
    spans
}

/// Push the accumulated plain text (if any) as a [`SpanStyle::Plain`] span and
/// clear the buffer.
fn flush(buf: &mut String, spans: &mut Vec<Span>) {
    if !buf.is_empty() {
        spans.push(Span::new(std::mem::take(buf), SpanStyle::Plain));
    }
}

/// The characters `chars[start..end]` collected into a `String`.
fn collect(chars: &[char], start: usize, end: usize) -> String {
    chars[start..end].iter().collect()
}

/// The index of the next `needle` at or after `start`, if any.
fn find_char(chars: &[char], start: usize, needle: char) -> Option<usize> {
    (start..chars.len()).find(|&k| chars[k] == needle)
}

/// The index of the first of a doubled `marker` pair at or after `start`, if any.
fn find_double(chars: &[char], start: usize, marker: char) -> Option<usize> {
    (start..chars.len().saturating_sub(1)).find(|&k| chars[k] == marker && chars[k + 1] == marker)
}

/// Parse a `[label](url)` link starting at `chars[start] == '['`. Returns the
/// label and the index just past the closing `)`.
fn parse_link(chars: &[char], start: usize) -> Option<(String, usize)> {
    let close_bracket = find_char(chars, start + 1, ']')?;
    if chars.get(close_bracket + 1) != Some(&'(') {
        return None;
    }
    let close_paren = find_char(chars, close_bracket + 2, ')')?;
    Some((collect(chars, start + 1, close_bracket), close_paren + 1))
}

#[cfg(test)]
mod tests;

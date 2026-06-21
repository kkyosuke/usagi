//! On-disk markdown (frontmatter) serialisation and parsing for [`Memory`].

use chrono::{DateTime, Utc};

use super::{Memory, MemoryType, ParseMemoryError};

impl Memory {
    /// Render this memory to its on-disk markdown representation.
    pub fn to_markdown(&self) -> String {
        let mut out = String::from("---\n");
        out.push_str(&format!("name: {}\n", inline(&self.name)));
        out.push_str(&format!("title: {}\n", inline(&self.title)));
        out.push_str(&format!("type: {}\n", self.kind.as_str()));
        out.push_str(&format!("related: {}\n", format_string_list(&self.related)));
        out.push_str(&format!("created_at: {}\n", self.created_at.to_rfc3339()));
        out.push_str(&format!("updated_at: {}\n", self.updated_at.to_rfc3339()));
        out.push_str("---\n\n");
        out.push_str(self.body.trim_end_matches('\n'));
        out.push('\n');
        out
    }

    /// Parse a memory from its on-disk markdown representation.
    pub fn from_markdown(text: &str) -> Result<Memory, ParseMemoryError> {
        let rest = text
            .strip_prefix("---\n")
            .or_else(|| text.strip_prefix("---\r\n"))
            .ok_or_else(|| ParseMemoryError("missing frontmatter opening '---'".to_string()))?;

        let (frontmatter, body) = split_frontmatter(rest)?;

        let mut name: Option<String> = None;
        let mut title: Option<String> = None;
        let mut kind = MemoryType::default();
        let mut related: Vec<String> = Vec::new();
        let mut created_at: Option<DateTime<Utc>> = None;
        let mut updated_at: Option<DateTime<Utc>> = None;

        for line in frontmatter.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let (key, value) = line
                .split_once(':')
                .ok_or_else(|| ParseMemoryError(format!("invalid frontmatter line: {line:?}")))?;
            let value = value.trim();
            match key.trim() {
                "name" => name = Some(value.to_string()),
                "title" => title = Some(value.to_string()),
                "type" => kind = value.parse()?,
                "related" => related = parse_string_list(value),
                "created_at" => created_at = Some(parse_timestamp(value)?),
                "updated_at" => updated_at = Some(parse_timestamp(value)?),
                // Unknown keys are ignored so the format can grow without
                // breaking older readers.
                _ => {}
            }
        }

        Ok(Memory {
            name: name.ok_or_else(|| ParseMemoryError("missing 'name'".to_string()))?,
            title: title.ok_or_else(|| ParseMemoryError("missing 'title'".to_string()))?,
            kind,
            related,
            created_at: created_at
                .ok_or_else(|| ParseMemoryError("missing 'created_at'".to_string()))?,
            updated_at: updated_at
                .ok_or_else(|| ParseMemoryError("missing 'updated_at'".to_string()))?,
            // Normalize the surrounding blank lines so the body round-trips with
            // `to_markdown`, which trims trailing newlines and inserts a single
            // blank line after the frontmatter.
            body: body
                .trim_start_matches(['\r', '\n'])
                .trim_end_matches(['\r', '\n'])
                .to_string(),
        })
    }
}

/// Split the text following the opening `---` into the frontmatter block and the
/// body that follows the closing `---` (a line consisting solely of `---`).
fn split_frontmatter(rest: &str) -> Result<(&str, &str), ParseMemoryError> {
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches('\n').trim_end_matches('\r') == "---" {
            let frontmatter = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return Ok((frontmatter, body));
        }
        offset += line.len();
    }
    Err(ParseMemoryError(
        "missing frontmatter closing '---'".to_string(),
    ))
}

/// Render strings as a `[a, b, c]` frontmatter list.
///
/// Each item is escaped (see [`escape_list_item`]) so the list round-trips
/// losslessly even when an item contains the delimiter (`,`), the list brackets
/// (`[` / `]`), a backslash, or boundary spaces. Items with none of those render
/// verbatim, keeping the common case readable (`[editor-config]`).
fn format_string_list(items: &[String]) -> String {
    let items: Vec<String> = items.iter().map(|s| escape_list_item(&inline(s))).collect();
    format!("[{}]", items.join(", "))
}

/// Backslash-escape the characters that are structural in a `[a, b, c]` list so
/// an item can carry them verbatim: `\` (the escape introducer), `,` (the item
/// delimiter), and `[` / `]` (the list brackets).
///
/// Leading and trailing spaces are encoded as `\s` so they survive the reader's
/// `trim()` (which exists only to drop the cosmetic space the `", "` join
/// inserts between items). Interior spaces stay literal, keeping common values
/// readable.
fn escape_list_item(item: &str) -> String {
    let after_leading = item.trim_start_matches(' ');
    let core = after_leading.trim_end_matches(' ');
    let leading = item.len() - after_leading.len();
    let trailing = after_leading.len() - core.len();

    let mut out = String::with_capacity(item.len());
    out.push_str(&"\\s".repeat(leading));
    for ch in core.chars() {
        if matches!(ch, '\\' | ',' | '[' | ']') {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push_str(&"\\s".repeat(trailing));
    out
}

/// Split a list body on its unescaped `,` delimiters, leaving escape sequences
/// intact for [`unescape_list_item`] to decode. A `\` consumes the next
/// character (whatever its byte width), so an escaped `\,` is not a delimiter; a
/// trailing `\` with no following character is kept so input never drops data.
fn split_escaped_list(inner: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut chars = inner.char_indices();
    while let Some((idx, ch)) = chars.next() {
        match ch {
            '\\' => {
                chars.next(); // skip whatever the backslash escapes
            }
            ',' => {
                parts.push(&inner[start..idx]);
                start = idx + 1; // ',' is one byte
            }
            _ => {}
        }
    }
    parts.push(&inner[start..]);
    parts
}

/// Decode the escape sequences produced by [`escape_list_item`]: `\\`, `\,`,
/// `\[`, `\]` become their literal character and `\s` becomes a space. A
/// backslash before any other character (or at end of input) is kept verbatim,
/// so unescaping a string with no escapes is a no-op (backward compatible with
/// simple lists written by older versions or by hand).
fn unescape_list_item(item: &str) -> String {
    let mut out = String::with_capacity(item.len());
    let mut chars = item.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.peek() {
                Some('\\' | ',' | '[' | ']') => out.push(chars.next().unwrap()),
                Some('s') => {
                    chars.next();
                    out.push(' ');
                }
                _ => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// Neutralise line breaks in a value bound for a single frontmatter line.
///
/// Frontmatter is line-based (`key: value`), so a newline in a value would split
/// it into a second line that the parser re-reads as a forged metadata field on
/// the next load. User-supplied text (the memory title via MCP `memory_save` and
/// the TUI) reaches these fields unvalidated, so the only characters that can
/// break the format — `\n` and `\r` — are replaced with a space here, at the
/// serialisation boundary.
fn inline(value: &str) -> String {
    value.replace(['\n', '\r'], " ")
}

/// Parse `[a, b, c]` (or a bare comma list) into non-empty strings.
///
/// Splitting respects the escaping applied by [`escape_list_item`]: a `\,` is a
/// literal comma inside an item rather than a delimiter, `\s` decodes to a
/// boundary space, and `\\` / `\[` / `\]` decode to their literal characters.
/// Each split part is trimmed (to drop the cosmetic `", "` join space) before
/// being unescaped, so simple lists like `[editor-config]` parse as before.
fn parse_string_list(value: &str) -> Vec<String> {
    let inner = value
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .unwrap_or(value);
    split_escaped_list(inner)
        .into_iter()
        .map(|s| unescape_list_item(s.trim()))
        .filter(|s| !s.is_empty())
        .collect()
}

/// Parse an RFC3339 timestamp into UTC.
fn parse_timestamp(value: &str) -> Result<DateTime<Utc>, ParseMemoryError> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| ParseMemoryError(format!("invalid timestamp: {value:?}")))
}

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
fn format_string_list(items: &[String]) -> String {
    let items: Vec<String> = items.iter().map(|s| inline(s)).collect();
    format!("[{}]", items.join(", "))
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

/// Parse `[a, b, c]` (or a bare comma list) into trimmed, non-empty strings.
fn parse_string_list(value: &str) -> Vec<String> {
    let inner = value
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .unwrap_or(value);
    inner
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Parse an RFC3339 timestamp into UTC.
fn parse_timestamp(value: &str) -> Result<DateTime<Utc>, ParseMemoryError> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| ParseMemoryError(format!("invalid timestamp: {value:?}")))
}

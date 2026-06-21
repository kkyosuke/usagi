//! On-disk markdown (frontmatter) serialisation and parsing for [`Issue`].

use chrono::{DateTime, Utc};

use super::{Issue, IssuePriority, IssueStatus, ParseIssueError};

impl Issue {
    /// Render this issue to its on-disk markdown representation.
    ///
    /// Required fields are always emitted; the optional `parent` and `milestone`
    /// lines are written only when set, so issues that don't use them stay clean.
    pub fn to_markdown(&self) -> String {
        let mut out = String::from("---\n");
        out.push_str(&format!("number: {}\n", self.number));
        out.push_str(&format!("title: {}\n", inline(&self.title)));
        out.push_str(&format!("status: {}\n", self.status.as_str()));
        out.push_str(&format!("priority: {}\n", self.priority.as_str()));
        out.push_str(&format!("labels: {}\n", format_string_list(&self.labels)));
        out.push_str(&format!(
            "dependson: {}\n",
            format_number_list(&self.dependson)
        ));
        out.push_str(&format!("related: {}\n", format_number_list(&self.related)));
        if let Some(parent) = self.parent {
            out.push_str(&format!("parent: {parent}\n"));
        }
        if let Some(milestone) = &self.milestone {
            out.push_str(&format!("milestone: {}\n", inline(milestone)));
        }
        out.push_str(&format!("created_at: {}\n", self.created_at.to_rfc3339()));
        out.push_str(&format!("updated_at: {}\n", self.updated_at.to_rfc3339()));
        out.push_str("---\n\n");
        out.push_str(self.body.trim_end_matches('\n'));
        out.push('\n');
        out
    }

    /// Parse an issue from its on-disk markdown representation.
    pub fn from_markdown(text: &str) -> Result<Issue, ParseIssueError> {
        let rest = text
            .strip_prefix("---\n")
            .or_else(|| text.strip_prefix("---\r\n"))
            .ok_or_else(|| ParseIssueError("missing frontmatter opening '---'".to_string()))?;

        let (frontmatter, body) = split_frontmatter(rest)?;

        let mut number: Option<u32> = None;
        let mut title: Option<String> = None;
        let mut status = IssueStatus::default();
        let mut priority = IssuePriority::default();
        let mut labels: Vec<String> = Vec::new();
        let mut dependson: Vec<u32> = Vec::new();
        let mut related: Vec<u32> = Vec::new();
        let mut parent: Option<u32> = None;
        let mut milestone: Option<String> = None;
        let mut created_at: Option<DateTime<Utc>> = None;
        let mut updated_at: Option<DateTime<Utc>> = None;

        for line in frontmatter.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let (key, value) = line
                .split_once(':')
                .ok_or_else(|| ParseIssueError(format!("invalid frontmatter line: {line:?}")))?;
            // Numeric / enum / list / timestamp fields are trimmed, but free-text
            // scalars (`title`, `milestone`) are written verbatim after a single
            // `key: ` delimiter space, so strip only that one space for them —
            // full trimming would drop the user's own leading/trailing spaces and
            // break the round-trip the list escaping is otherwise careful to keep.
            let text_value = value.strip_prefix(' ').unwrap_or(value);
            let value = value.trim();
            match key.trim() {
                "number" => {
                    number = Some(
                        value
                            .parse()
                            .map_err(|_| ParseIssueError(format!("invalid number: {value:?}")))?,
                    )
                }
                "title" => title = Some(text_value.to_string()),
                "status" => status = value.parse()?,
                "priority" => priority = value.parse()?,
                "labels" => labels = parse_string_list(value),
                "dependson" => dependson = parse_number_list(value)?,
                "related" => related = parse_number_list(value)?,
                "parent" => {
                    parent =
                        if value.is_empty() {
                            None
                        } else {
                            Some(value.parse().map_err(|_| {
                                ParseIssueError(format!("invalid parent: {value:?}"))
                            })?)
                        }
                }
                "milestone" => {
                    milestone = if value.is_empty() {
                        None
                    } else {
                        Some(text_value.to_string())
                    }
                }
                "created_at" => created_at = Some(parse_timestamp(value)?),
                "updated_at" => updated_at = Some(parse_timestamp(value)?),
                // Unknown keys are ignored so the format can grow without
                // breaking older readers.
                _ => {}
            }
        }

        Ok(Issue {
            number: number.ok_or_else(|| ParseIssueError("missing 'number'".to_string()))?,
            title: title.ok_or_else(|| ParseIssueError("missing 'title'".to_string()))?,
            status,
            priority,
            labels,
            dependson,
            related,
            parent,
            milestone,
            created_at: created_at
                .ok_or_else(|| ParseIssueError("missing 'created_at'".to_string()))?,
            updated_at: updated_at
                .ok_or_else(|| ParseIssueError("missing 'updated_at'".to_string()))?,
            // Normalize the surrounding blank lines so the body round-trips
            // with `to_markdown`, which trims trailing newlines and inserts a
            // single blank line after the frontmatter.
            body: body
                .trim_start_matches(['\r', '\n'])
                .trim_end_matches(['\r', '\n'])
                .to_string(),
        })
    }
}

/// Split the text following the opening `---` into the frontmatter block and
/// the body that follows the closing `---` (a line consisting solely of `---`,
/// with or without a trailing newline).
fn split_frontmatter(rest: &str) -> Result<(&str, &str), ParseIssueError> {
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches('\n').trim_end_matches('\r') == "---" {
            let frontmatter = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return Ok((frontmatter, body));
        }
        offset += line.len();
    }
    Err(ParseIssueError(
        "missing frontmatter closing '---'".to_string(),
    ))
}

/// Render strings as a `[a, b, c]` frontmatter list.
///
/// Each item is escaped (see [`escape_list_item`]) so the list round-trips
/// losslessly even when an item contains the delimiter (`,`), the list brackets
/// (`[` / `]`), a backslash, or boundary spaces. Items with none of those render
/// verbatim, keeping the common case readable (`[cli, infra]`).
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
/// like `a b` readable.
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
/// the next load (e.g. a title `"Fix\nstatus: done"` would inject a `status`).
/// User-supplied text (titles, labels, milestones via MCP `issue_create` and the
/// TUI) reaches these fields unvalidated, so the only characters that can break
/// the format — `\n` and `\r` — are replaced with a space here, at the
/// serialisation boundary.
fn inline(value: &str) -> String {
    value.replace(['\n', '\r'], " ")
}

/// Render numbers as a `[1, 2, 3]` frontmatter list.
fn format_number_list(items: &[u32]) -> String {
    format!(
        "[{}]",
        items
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Parse `[a, b, c]` (or a bare comma list) into non-empty strings.
///
/// Splitting respects the escaping applied by [`escape_list_item`]: a `\,` is a
/// literal comma inside an item rather than a delimiter, `\s` decodes to a
/// boundary space, and `\\` / `\[` / `\]` decode to their literal characters.
/// Each split part is trimmed (to drop the cosmetic `", "` join space) before
/// being unescaped, so simple lists like `[cli, infra]` parse exactly as before.
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

/// Parse `[1, 2, 3]` into issue numbers.
fn parse_number_list(value: &str) -> Result<Vec<u32>, ParseIssueError> {
    parse_string_list(value)
        .iter()
        .map(|s| {
            s.parse()
                .map_err(|_| ParseIssueError(format!("invalid dependson entry: {s:?}")))
        })
        .collect()
}

/// Parse an RFC3339 timestamp into UTC.
fn parse_timestamp(value: &str) -> Result<DateTime<Utc>, ParseIssueError> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| ParseIssueError(format!("invalid timestamp: {value:?}")))
}

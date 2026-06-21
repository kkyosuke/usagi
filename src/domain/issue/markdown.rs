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
            let value = value.trim();
            match key.trim() {
                "number" => {
                    number = Some(
                        value
                            .parse()
                            .map_err(|_| ParseIssueError(format!("invalid number: {value:?}")))?,
                    )
                }
                "title" => title = Some(value.to_string()),
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
                        Some(value.to_string())
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
fn format_string_list(items: &[String]) -> String {
    let items: Vec<String> = items.iter().map(|s| inline(s)).collect();
    format!("[{}]", items.join(", "))
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

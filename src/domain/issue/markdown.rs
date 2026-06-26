//! On-disk markdown (frontmatter) serialisation and parsing for [`Issue`].
//!
//! The frontmatter *format* (the `---` block, list escaping, timestamps, line
//! neutralisation) lives once in [`crate::domain::frontmatter`]; this module owns
//! only the issue's field set and the issue-specific number-list helpers.

use std::fmt::Write;

use chrono::{DateTime, Utc};

use crate::domain::frontmatter::{
    format_string_list, inline, parse_string_list, parse_timestamp, split_frontmatter,
};

use super::{Issue, IssuePriority, IssueStatus, ParseIssueError};

impl Issue {
    /// Render this issue to its on-disk markdown representation.
    ///
    /// Required fields are always emitted; the optional `parent` and `milestone`
    /// lines are written only when set, so issues that don't use them stay clean.
    pub fn to_markdown(&self) -> String {
        // Writing into a `String` via `std::fmt::Write` is infallible, so each
        // `writeln!` result is discarded. This appends straight into `out`
        // rather than allocating a throwaway `String` per field as
        // `push_str(&format!(…))` would (cf. `format_number_list`).
        let mut out = String::from("---\n");
        let _ = writeln!(out, "number: {}", self.number);
        let _ = writeln!(out, "title: {}", inline(&self.title));
        let _ = writeln!(out, "status: {}", self.status.as_str());
        let _ = writeln!(out, "priority: {}", self.priority.as_str());
        let _ = writeln!(out, "labels: {}", format_string_list(&self.labels));
        let _ = writeln!(out, "dependson: {}", format_number_list(&self.dependson));
        let _ = writeln!(out, "related: {}", format_number_list(&self.related));
        if let Some(parent) = self.parent {
            let _ = writeln!(out, "parent: {parent}");
        }
        if let Some(milestone) = &self.milestone {
            let _ = writeln!(out, "milestone: {}", inline(milestone));
        }
        let _ = writeln!(out, "created_at: {}", self.created_at.to_rfc3339());
        let _ = writeln!(out, "updated_at: {}", self.updated_at.to_rfc3339());
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

/// Render numbers as a `[1, 2, 3]` frontmatter list.
///
/// Writes the comma-separated numbers straight into the output string, avoiding
/// the intermediate `Vec<String>` an `iter().map(to_string).collect().join()`
/// would allocate on every `to_markdown`.
fn format_number_list(items: &[u32]) -> String {
    let mut out = String::from("[");
    for (i, n) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        // Writing a u32 into a String is infallible.
        let _ = write!(out, "{n}");
    }
    out.push(']');
    out
}

/// Parse `[1, 2, 3]` into issue numbers. Used for both `dependson` and
/// `related`, so the error stays field-agnostic rather than naming one field.
fn parse_number_list(value: &str) -> Result<Vec<u32>, ParseIssueError> {
    parse_string_list(value)
        .iter()
        .map(|s| {
            s.parse()
                .map_err(|_| ParseIssueError(format!("invalid issue number: {s:?}")))
        })
        .collect()
}

//! On-disk markdown (frontmatter) serialisation and parsing for [`Memory`].
//!
//! The frontmatter *format* (the `---` block, list escaping, timestamps, line
//! neutralisation) lives once in [`crate::domain::frontmatter`]; this module owns
//! only the memory's field set.

use std::fmt::Write;

use chrono::{DateTime, Utc};

use crate::domain::frontmatter::{
    format_string_list, inline, parse_string_list, parse_timestamp, split_frontmatter,
};

use super::{Memory, MemoryType, ParseMemoryError};

impl Memory {
    /// Render this memory to its on-disk markdown representation.
    pub fn to_markdown(&self) -> String {
        // Writing into a `String` via `std::fmt::Write` is infallible, so each
        // `writeln!` result is discarded. This appends straight into `out`
        // rather than allocating a throwaway `String` per field as
        // `push_str(&format!(…))` would.
        let mut out = String::from("---\n");
        let _ = writeln!(out, "name: {}", inline(&self.name));
        let _ = writeln!(out, "title: {}", inline(&self.title));
        let _ = writeln!(out, "type: {}", self.kind.as_str());
        let _ = writeln!(out, "related: {}", format_string_list(&self.related));
        let _ = writeln!(out, "created_at: {}", self.created_at.to_rfc3339());
        let _ = writeln!(out, "updated_at: {}", self.updated_at.to_rfc3339());
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
            // Free-text scalars (`name`, `title`) are written verbatim after a
            // single `key: ` delimiter space, so strip only that one space for
            // them; trimming would drop the user's own leading/trailing spaces and
            // break the round-trip. Enum / list / timestamp fields stay trimmed.
            let text_value = value.strip_prefix(' ').unwrap_or(value);
            let value = value.trim();
            match key.trim() {
                "name" => name = Some(text_value.to_string()),
                "title" => title = Some(text_value.to_string()),
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

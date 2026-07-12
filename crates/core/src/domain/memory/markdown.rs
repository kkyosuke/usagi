//! On-disk markdown (frontmatter) serialisation and parsing for [`Memory`].
//!
//! The frontmatter *format* (the `---` block, list escaping, timestamps, line
//! neutralisation) lives once in [`crate::domain::frontmatter`]; this module owns
//! only the memory's field set.

use std::fmt::Write;

use chrono::{DateTime, Utc};

use crate::domain::frontmatter::{
    self, FrontmatterDoc, format_string_list, inline, parse_string_list, parse_timestamp,
};

use super::{Memory, MemoryType, ParseMemoryError};

impl FrontmatterDoc for Memory {
    /// Render this memory to its on-disk markdown representation.
    ///
    /// The `---` envelope and body normalisation live in
    /// [`frontmatter::to_markdown`]; this closure only lists the memory's fields.
    fn to_markdown(&self) -> String {
        frontmatter::to_markdown(&self.body, |out| {
            let _ = writeln!(out, "name: {}", inline(&self.name));
            let _ = writeln!(out, "title: {}", inline(&self.title));
            let _ = writeln!(out, "type: {}", self.kind.as_str());
            let _ = writeln!(out, "related: {}", format_string_list(&self.related));
            let _ = writeln!(out, "created_at: {}", self.created_at.to_rfc3339());
            let _ = writeln!(out, "updated_at: {}", self.updated_at.to_rfc3339());
        })
    }

    /// Parse a memory from its on-disk markdown representation.
    ///
    /// The `---` envelope, line loop, and body normalisation live in
    /// [`frontmatter::from_markdown`]; this dispatcher only maps each field.
    /// Free-text scalars (`name`, `title`) take `text_value` so their own
    /// leading/trailing spaces survive the round-trip, while enum / list /
    /// timestamp fields work from the trimmed `value`.
    ///
    /// # Errors
    ///
    /// Returns [`ParseMemoryError`] when the frontmatter envelope is malformed, a
    /// field value fails to parse (enum token, timestamp), or a required field
    /// (`name`, `title`, `created_at`, `updated_at`) is absent.
    fn from_markdown(text: &str) -> Result<Memory, ParseMemoryError> {
        let mut name: Option<String> = None;
        let mut title: Option<String> = None;
        let mut kind = MemoryType::default();
        let mut related: Vec<String> = Vec::new();
        let mut created_at: Option<DateTime<Utc>> = None;
        let mut updated_at: Option<DateTime<Utc>> = None;

        let body = frontmatter::from_markdown(
            text,
            |key, value, text_value| -> Result<(), ParseMemoryError> {
                match key {
                    "name" => name = Some(text_value.to_string()),
                    "title" => title = Some(text_value.to_string()),
                    "type" => kind = value.parse()?,
                    "related" => related = parse_string_list(value),
                    "created_at" => created_at = Some(parse_timestamp(value)?),
                    "updated_at" => updated_at = Some(parse_timestamp(value)?),
                    _ => {}
                }
                Ok(())
            },
        )?;

        Ok(Memory {
            name: name.ok_or_else(|| ParseMemoryError("missing 'name'".to_string()))?,
            title: title.ok_or_else(|| ParseMemoryError("missing 'title'".to_string()))?,
            kind,
            related,
            created_at: created_at
                .ok_or_else(|| ParseMemoryError("missing 'created_at'".to_string()))?,
            updated_at: updated_at
                .ok_or_else(|| ParseMemoryError("missing 'updated_at'".to_string()))?,
            body,
        })
    }
}

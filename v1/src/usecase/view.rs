//! Shared conventions for the serde views the CLI (`--json`) and MCP
//! presentations emit for domain entities.
//!
//! Each entity (issue, memory, …) keeps its own `#[derive(Serialize)]` view
//! struct because the field sets differ, but they all follow one shape: a
//! borrowing view built by `From<&Entity>` whose `created_at` / `updated_at`
//! timestamps are rendered as owned `String`s via [`timestamp`]. Centralising
//! that timestamp rule here keeps the JSON surface's date format defined in a
//! single place rather than as a `to_rfc3339` call copied into every `From`
//! impl. Both surfaces consume the views via `serde_json` (`to_string_pretty` /
//! `to_value`).

use chrono::{DateTime, Utc};

/// Render a timestamp for the JSON surface: RFC3339 with a `+00:00` offset (via
/// [`chrono::DateTime::to_rfc3339`]), matching how
/// [`crate::domain::frontmatter`] persists and parses timestamps.
pub fn timestamp(ts: &DateTime<Utc>) -> String {
    ts.to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn timestamp_renders_rfc3339_with_utc_offset() {
        let ts = Utc.with_ymd_and_hms(2026, 6, 14, 1, 2, 3).unwrap();
        assert_eq!(timestamp(&ts), "2026-06-14T01:02:03+00:00");
    }
}

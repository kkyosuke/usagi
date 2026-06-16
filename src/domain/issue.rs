//! The `Issue` entity: a unit of task tracking persisted as a frontmatter
//! markdown file under `<repo>/.usagi/issues/`.
//!
//! Each issue is a single `NNN-<slug>.md` file whose top block is a small,
//! line-based frontmatter (the metadata) followed by a free-form markdown body.
//! The format mirrors the hand-written issues this project already keeps under
//! `issues/`, so the same files read well to both humans and agents.
//!
//! Parsing and serialization are hand-rolled over a fixed, known set of fields
//! rather than pulling in a YAML crate: the project standardizes on JSON for
//! machine data (see `document/`), and a focused parser keeps the dependency
//! surface small while staying fully testable.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Where an issue sits in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IssueStatus {
    /// Not started.
    #[default]
    Todo,
    /// Being worked on.
    InProgress,
    /// Finished.
    Done,
}

impl IssueStatus {
    /// The on-disk / display token for this status.
    pub fn as_str(&self) -> &'static str {
        match self {
            IssueStatus::Todo => "todo",
            IssueStatus::InProgress => "in-progress",
            IssueStatus::Done => "done",
        }
    }
}

impl fmt::Display for IssueStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for IssueStatus {
    type Err = ParseIssueError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "todo" => Ok(IssueStatus::Todo),
            "in-progress" => Ok(IssueStatus::InProgress),
            "done" => Ok(IssueStatus::Done),
            other => Err(ParseIssueError(format!("invalid status: {other:?}"))),
        }
    }
}

/// How urgent an issue is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IssuePriority {
    High,
    #[default]
    Medium,
    Low,
}

impl IssuePriority {
    /// The on-disk / display token for this priority.
    pub fn as_str(&self) -> &'static str {
        match self {
            IssuePriority::High => "high",
            IssuePriority::Medium => "medium",
            IssuePriority::Low => "low",
        }
    }
}

impl fmt::Display for IssuePriority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for IssuePriority {
    type Err = ParseIssueError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "high" => Ok(IssuePriority::High),
            "medium" => Ok(IssuePriority::Medium),
            "low" => Ok(IssuePriority::Low),
            other => Err(ParseIssueError(format!("invalid priority: {other:?}"))),
        }
    }
}

/// A single tracked task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// Stable, monotonically assigned number (also the filename prefix).
    pub number: u32,
    pub title: String,
    pub status: IssueStatus,
    pub priority: IssuePriority,
    /// Free-form labels.
    pub labels: Vec<String>,
    /// Numbers of issues that must be `done` before this one can start.
    pub dependson: Vec<u32>,
    /// Numbers of issues related to this one without blocking it (a soft,
    /// non-blocking cross-reference, unlike `dependson`).
    pub related: Vec<u32>,
    /// Number of the parent issue this one belongs to (an epic ⊃ sub-task
    /// hierarchy), if any. Distinct from `dependson`, which is a precondition.
    pub parent: Option<u32>,
    /// Milestone this issue is grouped under, if any.
    pub milestone: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Markdown body below the frontmatter.
    pub body: String,
}

/// Lightweight metadata view of an [`Issue`] — everything except the body — as
/// stored in the JSON index and surfaced by listings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueSummary {
    pub number: u32,
    pub title: String,
    pub status: IssueStatus,
    pub priority: IssuePriority,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub dependson: Vec<u32>,
    #[serde(default)]
    pub related: Vec<u32>,
    #[serde(default)]
    pub parent: Option<u32>,
    #[serde(default)]
    pub milestone: Option<String>,
    /// File name (relative to the issues directory) backing this issue.
    pub file: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// An error parsing an issue's markdown frontmatter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseIssueError(pub String);

impl fmt::Display for ParseIssueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ParseIssueError {}

impl Issue {
    /// A filename-safe slug derived from the title: lowercase, with every run of
    /// non-alphanumeric characters collapsed to a single hyphen. Falls back to
    /// `"issue"` when the title has no usable characters.
    pub fn slug(&self) -> String {
        let mut slug = String::new();
        let mut prev_dash = false;
        for ch in self.title.chars() {
            if ch.is_ascii_alphanumeric() {
                slug.push(ch.to_ascii_lowercase());
                prev_dash = false;
            } else if !prev_dash {
                slug.push('-');
                prev_dash = true;
            }
        }
        let trimmed = slug.trim_matches('-');
        if trimmed.is_empty() {
            "issue".to_string()
        } else {
            trimmed.to_string()
        }
    }

    /// The file name backing this issue, e.g. `001-add-doctor.md`.
    pub fn file_name(&self) -> String {
        format!("{:03}-{}.md", self.number, self.slug())
    }

    /// Build the metadata summary for this issue.
    pub fn summary(&self) -> IssueSummary {
        IssueSummary {
            number: self.number,
            title: self.title.clone(),
            status: self.status,
            priority: self.priority,
            labels: self.labels.clone(),
            dependson: self.dependson.clone(),
            related: self.related.clone(),
            parent: self.parent,
            milestone: self.milestone.clone(),
            file: self.file_name(),
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }

    /// Render this issue to its on-disk markdown representation.
    ///
    /// Required fields are always emitted; the optional `parent` and `milestone`
    /// lines are written only when set, so issues that don't use them stay clean.
    pub fn to_markdown(&self) -> String {
        let mut out = String::from("---\n");
        out.push_str(&format!("number: {}\n", self.number));
        out.push_str(&format!("title: {}\n", self.title));
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
            out.push_str(&format!("milestone: {milestone}\n"));
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
    format!("[{}]", items.join(", "))
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample() -> Issue {
        let ts = Utc.with_ymd_and_hms(2026, 6, 14, 1, 2, 3).unwrap();
        Issue {
            number: 7,
            title: "Add doctor command".to_string(),
            status: IssueStatus::InProgress,
            priority: IssuePriority::High,
            labels: vec!["cli".to_string(), "infra".to_string()],
            dependson: vec![1, 2],
            related: vec![3],
            parent: Some(4),
            milestone: Some("v1".to_string()),
            created_at: ts,
            updated_at: ts,
            body: "## Summary\n\nDo the thing.".to_string(),
        }
    }

    #[test]
    fn status_round_trips_through_string() {
        for s in [
            IssueStatus::Todo,
            IssueStatus::InProgress,
            IssueStatus::Done,
        ] {
            assert_eq!(s.as_str().parse::<IssueStatus>().unwrap(), s);
            assert_eq!(s.to_string(), s.as_str());
        }
        assert!("nope".parse::<IssueStatus>().is_err());
    }

    #[test]
    fn priority_round_trips_through_string() {
        for p in [
            IssuePriority::High,
            IssuePriority::Medium,
            IssuePriority::Low,
        ] {
            assert_eq!(p.as_str().parse::<IssuePriority>().unwrap(), p);
            assert_eq!(p.to_string(), p.as_str());
        }
        assert!("nope".parse::<IssuePriority>().is_err());
    }

    #[test]
    fn defaults_are_todo_and_medium() {
        assert_eq!(IssueStatus::default(), IssueStatus::Todo);
        assert_eq!(IssuePriority::default(), IssuePriority::Medium);
    }

    #[test]
    fn slug_collapses_punctuation_and_lowercases() {
        let mut issue = sample();
        issue.title = "Fix:  the AWS-SSO   login!".to_string();
        assert_eq!(issue.slug(), "fix-the-aws-sso-login");
    }

    #[test]
    fn slug_falls_back_when_title_has_no_alphanumerics() {
        let mut issue = sample();
        issue.title = "!!! ???".to_string();
        assert_eq!(issue.slug(), "issue");
    }

    #[test]
    fn file_name_zero_pads_the_number() {
        let issue = sample();
        assert_eq!(issue.file_name(), "007-add-doctor-command.md");
    }

    #[test]
    fn summary_mirrors_the_issue_without_body() {
        let issue = sample();
        let summary = issue.summary();
        assert_eq!(summary.number, 7);
        assert_eq!(summary.title, "Add doctor command");
        assert_eq!(summary.status, IssueStatus::InProgress);
        assert_eq!(summary.priority, IssuePriority::High);
        assert_eq!(summary.labels, vec!["cli", "infra"]);
        assert_eq!(summary.dependson, vec![1, 2]);
        assert_eq!(summary.related, vec![3]);
        assert_eq!(summary.parent, Some(4));
        assert_eq!(summary.milestone, Some("v1".to_string()));
        assert_eq!(summary.file, "007-add-doctor-command.md");
    }

    #[test]
    fn markdown_round_trips() {
        let issue = sample();
        let text = issue.to_markdown();
        let parsed = Issue::from_markdown(&text).unwrap();
        assert_eq!(parsed, issue);
    }

    #[test]
    fn markdown_renders_expected_shape() {
        let issue = sample();
        let text = issue.to_markdown();
        assert!(text.starts_with("---\nnumber: 7\ntitle: Add doctor command\n"));
        assert!(text.contains("status: in-progress\n"));
        assert!(text.contains("labels: [cli, infra]\n"));
        assert!(text.contains("dependson: [1, 2]\n"));
        assert!(text.contains("related: [3]\n"));
        assert!(text.contains("parent: 4\n"));
        assert!(text.contains("milestone: v1\n"));
        assert!(text.ends_with("## Summary\n\nDo the thing.\n"));
    }

    #[test]
    fn empty_labels_and_deps_round_trip() {
        let mut issue = sample();
        issue.labels.clear();
        issue.dependson.clear();
        issue.related.clear();
        let text = issue.to_markdown();
        assert!(text.contains("labels: []\n"));
        assert!(text.contains("dependson: []\n"));
        assert!(text.contains("related: []\n"));
        assert_eq!(Issue::from_markdown(&text).unwrap(), issue);
    }

    #[test]
    fn absent_parent_and_milestone_are_omitted_and_round_trip() {
        let mut issue = sample();
        issue.parent = None;
        issue.milestone = None;
        let text = issue.to_markdown();
        assert!(!text.contains("parent:"));
        assert!(!text.contains("milestone:"));
        assert_eq!(Issue::from_markdown(&text).unwrap(), issue);
    }

    #[test]
    fn blank_parent_and_milestone_values_parse_as_none() {
        let text = "---\nnumber: 1\ntitle: T\nstatus: todo\npriority: medium\n\
             parent: \nmilestone: \ncreated_at: 2026-06-14T00:00:00Z\n\
             updated_at: 2026-06-14T00:00:00Z\n---\n";
        let issue = Issue::from_markdown(text).unwrap();
        assert_eq!(issue.parent, None);
        assert_eq!(issue.milestone, None);
    }

    #[test]
    fn parse_rejects_a_non_numeric_parent() {
        let text = "---\nnumber: 1\ntitle: T\nstatus: todo\npriority: medium\n\
             parent: nope\ncreated_at: 2026-06-14T00:00:00Z\n\
             updated_at: 2026-06-14T00:00:00Z\n---\n";
        assert!(Issue::from_markdown(text)
            .unwrap_err()
            .to_string()
            .contains("invalid parent"));
    }

    #[test]
    fn parse_tolerates_blank_lines_unknown_keys_and_crlf() {
        let text = "---\r\n\
            number: 12\r\n\
            \r\n\
            title: Weird: but valid\r\n\
            status: done\r\n\
            priority: low\r\n\
            labels: [a]\r\n\
            dependson: []\r\n\
            future_field: ignored\r\n\
            created_at: 2026-06-14T00:00:00Z\r\n\
            updated_at: 2026-06-14T00:00:00Z\r\n\
            ---\r\n\
            \r\n\
            Body here.\r\n";
        let issue = Issue::from_markdown(text).unwrap();
        assert_eq!(issue.number, 12);
        // The title keeps everything after the first colon.
        assert_eq!(issue.title, "Weird: but valid");
        assert_eq!(issue.status, IssueStatus::Done);
        assert_eq!(issue.labels, vec!["a"]);
        assert!(issue.body.starts_with("Body here."));
    }

    #[test]
    fn parse_accepts_closing_fence_without_trailing_newline() {
        let text = "---\n\
            number: 1\n\
            title: T\n\
            status: todo\n\
            priority: medium\n\
            created_at: 2026-06-14T00:00:00Z\n\
            updated_at: 2026-06-14T00:00:00Z\n\
            ---";
        let issue = Issue::from_markdown(text).unwrap();
        assert_eq!(issue.number, 1);
        assert_eq!(issue.body, "");
        // Missing labels/dependson default to empty.
        assert!(issue.labels.is_empty());
        assert!(issue.dependson.is_empty());
    }

    #[test]
    fn parse_rejects_missing_opening_fence() {
        let err = Issue::from_markdown("number: 1\n").unwrap_err();
        assert!(err.to_string().contains("opening"));
    }

    #[test]
    fn parse_rejects_missing_closing_fence() {
        let text = "---\nnumber: 1\ntitle: T\n";
        let err = Issue::from_markdown(text).unwrap_err();
        assert!(err.to_string().contains("closing"));
    }

    #[test]
    fn parse_rejects_line_without_colon() {
        let text = "---\nnonsense\n---\n";
        let err = Issue::from_markdown(text).unwrap_err();
        assert!(err.to_string().contains("invalid frontmatter line"));
    }

    #[test]
    fn parse_rejects_bad_scalar_values() {
        // Non-numeric issue number.
        let bad_number = "---\nnumber: zzz\ntitle: T\nstatus: todo\npriority: medium\n\
             created_at: 2026-06-14T00:00:00Z\nupdated_at: 2026-06-14T00:00:00Z\n---\n";
        assert!(Issue::from_markdown(bad_number)
            .unwrap_err()
            .to_string()
            .contains("invalid number"));

        // Non-numeric dependency entry.
        let bad_dep = "---\nnumber: 1\ntitle: T\nstatus: todo\npriority: medium\n\
             dependson: [x]\ncreated_at: 2026-06-14T00:00:00Z\nupdated_at: 2026-06-14T00:00:00Z\n---\n";
        assert!(Issue::from_markdown(bad_dep)
            .unwrap_err()
            .to_string()
            .contains("invalid dependson"));

        // Unparseable timestamp.
        let bad_date = "---\nnumber: 1\ntitle: T\nstatus: todo\npriority: medium\n\
             created_at: not-a-date\nupdated_at: 2026-06-14T00:00:00Z\n---\n";
        assert!(Issue::from_markdown(bad_date)
            .unwrap_err()
            .to_string()
            .contains("invalid timestamp"));

        // Invalid status/priority tokens.
        let bad_status = "---\nnumber: 1\ntitle: T\nstatus: nope\npriority: medium\n\
             created_at: 2026-06-14T00:00:00Z\nupdated_at: 2026-06-14T00:00:00Z\n---\n";
        assert!(Issue::from_markdown(bad_status).is_err());
    }

    #[test]
    fn parse_rejects_missing_required_fields() {
        // Missing title.
        let text = "---\nnumber: 1\nstatus: todo\npriority: medium\n\
             created_at: 2026-06-14T00:00:00Z\nupdated_at: 2026-06-14T00:00:00Z\n---\n";
        assert!(Issue::from_markdown(text)
            .unwrap_err()
            .to_string()
            .contains("title"));

        // Missing number.
        let text = "---\ntitle: T\nstatus: todo\npriority: medium\n\
             created_at: 2026-06-14T00:00:00Z\nupdated_at: 2026-06-14T00:00:00Z\n---\n";
        assert!(Issue::from_markdown(text)
            .unwrap_err()
            .to_string()
            .contains("number"));

        // Missing created_at.
        let text = "---\nnumber: 1\ntitle: T\nstatus: todo\npriority: medium\n\
             updated_at: 2026-06-14T00:00:00Z\n---\n";
        assert!(Issue::from_markdown(text)
            .unwrap_err()
            .to_string()
            .contains("created_at"));

        // Missing updated_at.
        let text = "---\nnumber: 1\ntitle: T\nstatus: todo\npriority: medium\n\
             created_at: 2026-06-14T00:00:00Z\n---\n";
        assert!(Issue::from_markdown(text)
            .unwrap_err()
            .to_string()
            .contains("updated_at"));
    }

    #[test]
    fn summary_serializes_to_json() {
        let summary = sample().summary();
        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("\"status\":\"in-progress\""));
        let back: IssueSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back, summary);
    }
}

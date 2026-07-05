//! Shared serialisation and parsing for the line-based `key: value` frontmatter
//! block that both the [`crate::domain::issue::Issue`] and
//! [`crate::domain::memory::Memory`] entities persist.
//!
//! The two entities store different field sets but share one frontmatter
//! *format*: a `---`-delimited header of `key: value` lines, with list values
//! rendered as `[a, b, c]` using a small backslash escape scheme, timestamps in
//! RFC3339, and line breaks neutralised so a value can never forge a second
//! field. Keeping that format in a single place is what stops the issue and
//! memory files from silently drifting apart (e.g. one gaining an escape the
//! other lacks). Each entity parses its own fields and converts the
//! [`ParseFrontmatterError`] raised here into its own `Parse*Error` via `From`.

use std::fmt;

use chrono::{DateTime, Utc};

/// An error parsing a frontmatter block. Each entity converts this into its own
/// `Parse*Error` (see the `From` impls in `domain::issue` / `domain::memory`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseFrontmatterError(pub String);

impl fmt::Display for ParseFrontmatterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ParseFrontmatterError {}

/// An error parsing an entity's markdown frontmatter (an issue or a memory).
///
/// Both entities share one parse-error type: the message is the sole payload, so
/// distinct newtypes bought nothing but duplicated `Display` / `Error` / `From`
/// boilerplate. Each entity re-exports this under its own historical name
/// ([`crate::domain::issue::ParseIssueError`] /
/// [`crate::domain::memory::ParseMemoryError`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ParseError {}

impl From<ParseFrontmatterError> for ParseError {
    fn from(e: ParseFrontmatterError) -> Self {
        ParseError(e.0)
    }
}

/// Generate the `as_str` / [`Display`](std::fmt::Display) /
/// [`FromStr`](std::str::FromStr) string-token trio for a fieldless enum whose
/// variants map one-to-one to on-disk / display tokens.
///
/// The caller supplies the enum type, its [`FromStr::Err`](std::str::FromStr)
/// type, the noun used in the `invalid <noun>: ...` parse error, and the
/// variant → token table. The enum itself (with its `serde` derives and
/// `#[default]`) stays hand-written; only the three string impls — which were
/// near-identical copies across `IssueStatus` / `IssuePriority` / `MemoryType` /
/// `GroupBy` — are generated.
macro_rules! str_enum {
    ($name:ident, $err:path, $noun:literal, { $($variant:ident => $token:literal),+ $(,)? }) => {
        impl $name {
            /// The on-disk / display token for this value.
            pub fn as_str(&self) -> &'static str {
                match self {
                    $($name::$variant => $token,)+
                }
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = $err;

            fn from_str(s: &str) -> ::std::result::Result<Self, Self::Err> {
                match s.trim() {
                    $($token => ::std::result::Result::Ok($name::$variant),)+
                    other => ::std::result::Result::Err($err(format!(
                        concat!("invalid ", $noun, ": {:?}"),
                        other
                    ))),
                }
            }
        }
    };
}

pub(crate) use str_enum;

/// Turn an arbitrary string into a filename-safe slug: lowercase, with every run
/// of non-alphanumeric characters collapsed to a single hyphen. Falls back to
/// `fallback` when the input has no usable characters.
pub fn slugify(text: &str, fallback: &str) -> String {
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in text.chars() {
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
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Split the text following the opening `---` into the frontmatter block and the
/// body that follows the closing `---` (a line consisting solely of `---`, with
/// or without a trailing newline).
pub fn split_frontmatter(rest: &str) -> Result<(&str, &str), ParseFrontmatterError> {
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches('\n').trim_end_matches('\r') == "---" {
            let frontmatter = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return Ok((frontmatter, body));
        }
        offset += line.len();
    }
    Err(ParseFrontmatterError(
        "missing frontmatter closing '---'".to_string(),
    ))
}

/// Render strings as a `[a, b, c]` frontmatter list.
///
/// Each item is escaped (see [`escape_list_item`]) so the list round-trips
/// losslessly even when an item contains the delimiter (`,`), the list brackets
/// (`[` / `]`), a backslash, or boundary spaces. Items with none of those render
/// verbatim, keeping the common case readable (`[cli, infra]`).
pub fn format_string_list(items: &[String]) -> String {
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
/// User-supplied text (titles, labels, milestones via MCP and the TUI) reaches
/// these fields unvalidated, so the only characters that can break the format —
/// `\n` and `\r` — are replaced with a space here, at the serialisation boundary.
pub fn inline(value: &str) -> String {
    value.replace(['\n', '\r'], " ")
}

/// Parse `[a, b, c]` (or a bare comma list) into non-empty strings.
///
/// Splitting respects the escaping applied by [`escape_list_item`]: a `\,` is a
/// literal comma inside an item rather than a delimiter, `\s` decodes to a
/// boundary space, and `\\` / `\[` / `\]` decode to their literal characters.
/// Each split part is trimmed (to drop the cosmetic `", "` join space) before
/// being unescaped, so simple lists like `[cli, infra]` parse exactly as before.
pub fn parse_string_list(value: &str) -> Vec<String> {
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
pub fn parse_timestamp(value: &str) -> Result<DateTime<Utc>, ParseFrontmatterError> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| ParseFrontmatterError(format!("invalid timestamp: {value:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_lowercases_collapses_and_falls_back() {
        assert_eq!(
            slugify("Fix the AWS SSO login!", "issue"),
            "fix-the-aws-sso-login"
        );
        assert_eq!(slugify("already-kebab", "memory"), "already-kebab");
        // No usable characters falls back to the caller's default.
        assert_eq!(slugify("!!!", "issue"), "issue");
        assert_eq!(slugify("", "memory"), "memory");
    }

    #[test]
    fn split_frontmatter_separates_header_and_body() {
        let (fm, body) = split_frontmatter("a: 1\n---\nbody\n").unwrap();
        assert_eq!(fm, "a: 1\n");
        assert_eq!(body, "body\n");
    }

    #[test]
    fn split_frontmatter_errors_without_a_closing_delimiter() {
        let err = split_frontmatter("a: 1\nno close\n").unwrap_err();
        assert!(err.to_string().contains("missing frontmatter closing"));
    }

    #[test]
    fn string_lists_round_trip_through_escaping() {
        let items = vec![
            "plain".to_string(),
            "with, comma".to_string(),
            "[brackets]".to_string(),
            "back\\slash".to_string(),
            " spaced ".to_string(),
        ];
        let rendered = format_string_list(&items);
        assert_eq!(parse_string_list(&rendered), items);
    }

    #[test]
    fn parse_string_list_accepts_a_bare_list_and_drops_empties() {
        assert_eq!(
            parse_string_list("a, , b"),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn inline_neutralises_line_breaks() {
        assert_eq!(inline("a\nb\r\nc"), "a b  c");
    }

    #[test]
    fn parse_timestamp_reads_rfc3339_and_rejects_garbage() {
        assert!(parse_timestamp("2026-06-21T00:00:00+00:00").is_ok());
        assert!(parse_timestamp("not a timestamp")
            .unwrap_err()
            .to_string()
            .contains("invalid timestamp"));
    }
}

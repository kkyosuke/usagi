//! The output log's line model: a line and the kind that decides its colour.

/// The kind of a log line, which decides how it is coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// A command the user entered (echoed back).
    Command,
    /// Ordinary command output.
    Output,
    /// An error (e.g. an unknown command).
    Error,
    /// A transient notice (e.g. a "coming soon" message).
    Notice,
}

/// A single line in the output log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub kind: LineKind,
    pub text: String,
}

impl LogLine {
    pub fn command(text: impl Into<String>) -> Self {
        Self {
            kind: LineKind::Command,
            text: text.into(),
        }
    }

    pub fn output(text: impl Into<String>) -> Self {
        Self {
            kind: LineKind::Output,
            text: text.into(),
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            kind: LineKind::Error,
            text: text.into(),
        }
    }

    pub fn notice(text: impl Into<String>) -> Self {
        Self {
            kind: LineKind::Notice,
            text: text.into(),
        }
    }
}

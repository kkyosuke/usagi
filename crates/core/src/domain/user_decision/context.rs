//! Bounded explanatory material accompanying a human decision.

use serde::{Deserialize, Serialize};

use super::{UserDecisionError, UserDecisionPolicy};
use crate::domain::presentation_text::presentation_character_is_safe;

/// A comparison table or a preformatted, terminal-readable diagram.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UserDecisionContext {
    Table {
        title: String,
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Diagram {
        title: String,
        text: String,
    },
}

impl UserDecisionContext {
    pub(super) fn validate(&self) -> Result<(), UserDecisionError> {
        let nonempty =
            |text: &str, limit| !text.trim().is_empty() && text.len() <= limit && safe_text(text);
        let valid = match self {
            Self::Table {
                title,
                columns,
                rows,
            } => {
                nonempty(title, UserDecisionPolicy::TITLE_MAX_BYTES)
                    && !columns.is_empty()
                    && columns.len() <= UserDecisionPolicy::TABLE_COLUMNS_MAX
                    && columns
                        .iter()
                        .all(|column| nonempty(column, UserDecisionPolicy::CONTEXT_CELL_MAX_BYTES))
                    && !rows.is_empty()
                    && rows.len() <= UserDecisionPolicy::TABLE_ROWS_MAX
                    && rows.iter().all(|row| {
                        row.len() == columns.len()
                            && row.iter().all(|cell| {
                                cell.len() <= UserDecisionPolicy::CONTEXT_CELL_MAX_BYTES
                                    && safe_text(cell)
                            })
                    })
            }
            Self::Diagram { title, text } => {
                nonempty(title, UserDecisionPolicy::TITLE_MAX_BYTES)
                    && nonempty(text, UserDecisionPolicy::DIAGRAM_MAX_BYTES)
            }
        };
        valid.then_some(()).ok_or(UserDecisionError::InvalidRequest)
    }
}

fn safe_text(text: &str) -> bool {
    text.chars()
        .all(|ch| ch == '\n' || presentation_character_is_safe(ch))
}

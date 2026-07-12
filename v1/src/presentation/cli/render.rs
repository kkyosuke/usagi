//! Output helpers shared by the `usagi` CLI subcommands.

use anyhow::Result;
use serde::Serialize;

/// Serialize `value` to pretty JSON and return it split into lines, the shape
/// every `--json` listing prints. Shared by the subcommands so they emit
/// identically formatted JSON.
pub(crate) fn json_lines<T: Serialize>(value: &T) -> Result<Vec<String>> {
    let text = serde_json::to_string_pretty(value)?;
    Ok(text.lines().map(str::to_string).collect())
}

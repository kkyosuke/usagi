//! Output formatting for `usagi memory`: human-readable listings and the
//! `--json` serialisations.

use anyhow::Result;

use crate::domain::memory::MemorySummary;
use crate::presentation::cli::render::json_lines;
use crate::usecase::memory::MemorySummaryView;

/// Render a listing (from `list` or `search`) either as JSON or as aligned
/// human-readable lines.
pub(super) fn render_listing(items: Vec<MemorySummary>, json: bool) -> Result<Vec<String>> {
    if json {
        let views: Vec<MemorySummaryView> = items.iter().map(MemorySummaryView::from).collect();
        return json_lines(&views);
    }
    Ok(render_list(&items))
}

/// Format a listing as aligned, one-line-per-memory text.
fn render_list(items: &[MemorySummary]) -> Vec<String> {
    if items.is_empty() {
        return vec!["No memories found.".to_string()];
    }
    items
        .iter()
        .map(|s| format!("{:<12} {:<24} {}", s.kind.as_str(), s.name, s.title))
        .collect()
}

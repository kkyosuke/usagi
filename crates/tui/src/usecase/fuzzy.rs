//! Small, deterministic fuzzy matching shared by TUI pickers.

/// Rank a case-insensitive subsequence match.
///
/// Contiguous matches sort before gapped matches, then shorter gaps and earlier
/// starts win. Callers retain source order as the final stable tie-break.
#[must_use]
pub(crate) fn fuzzy_score(candidate: &str, query: &str) -> Option<usize> {
    let query = query.to_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    let candidate = candidate.to_lowercase();
    if let Some(start) = candidate.find(&query) {
        return Some(start);
    }

    let mut positions = Vec::new();
    let mut query_chars = query.chars();
    let mut wanted = query_chars.next()?;
    for (position, character) in candidate.chars().enumerate() {
        if character == wanted {
            positions.push(position);
            let Some(next) = query_chars.next() else {
                let start = positions[0];
                let gaps = position + 1 - start - positions.len();
                return Some(candidate.len() + gaps * 4 + start);
            };
            wanted = next;
        }
    }
    None
}

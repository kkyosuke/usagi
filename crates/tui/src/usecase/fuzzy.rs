//! Small, deterministic fuzzy matching shared by TUI pickers.

/// A ranked match together with the candidate positions the query landed on.
///
/// Positions are `char` indices into the original candidate in ascending order,
/// so a picker can highlight exactly the cells the query explains. Scores are
/// compared only between candidates of the same query; their absolute value
/// carries no meaning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FuzzyMatch {
    score: usize,
    positions: Vec<usize>,
}

impl FuzzyMatch {
    /// Rank of this match; lower sorts first.
    pub(crate) const fn score(&self) -> usize {
        self.score
    }

    /// Ascending `char` indices of the candidate that the query matched.
    pub(crate) fn positions(&self) -> &[usize] {
        &self.positions
    }
}

/// Rank a case-insensitive subsequence match.
///
/// Contiguous matches sort before gapped matches, then shorter gaps and earlier
/// starts win. Callers retain source order as the final stable tie-break.
#[must_use]
pub(crate) fn fuzzy_score(candidate: &str, query: &str) -> Option<usize> {
    fuzzy_match(candidate, query).map(|matched| matched.score)
}

/// Rank a case-insensitive subsequence match and report where it landed.
///
/// This is [`fuzzy_score`] with the matched positions retained; both share one
/// implementation so a picker that highlights cannot drift from one that only
/// ranks. An empty query matches everything at score `0` with no positions.
#[must_use]
pub(crate) fn fuzzy_match(candidate: &str, query: &str) -> Option<FuzzyMatch> {
    let query = query.chars().map(lowercase).collect::<Vec<_>>();
    if query.is_empty() {
        return Some(FuzzyMatch {
            score: 0,
            positions: Vec::new(),
        });
    }
    let candidate = candidate.chars().map(lowercase).collect::<Vec<_>>();
    if let Some(start) = contiguous_start(&candidate, &query) {
        return Some(FuzzyMatch {
            score: start,
            positions: (start..start + query.len()).collect(),
        });
    }

    let mut positions = Vec::new();
    let mut wanted = 0;
    for (position, character) in candidate.iter().enumerate() {
        if *character != query[wanted] {
            continue;
        }
        positions.push(position);
        wanted += 1;
        if wanted == query.len() {
            let start = positions[0];
            let gaps = position + 1 - start - positions.len();
            return Some(FuzzyMatch {
                score: candidate.len() + gaps * 4 + start,
                positions,
            });
        }
    }
    None
}

/// First index where `query` appears as a contiguous run inside `candidate`.
fn contiguous_start(candidate: &[char], query: &[char]) -> Option<usize> {
    if query.len() > candidate.len() {
        return None;
    }
    candidate
        .windows(query.len())
        .position(|window| window == query)
}

/// Lowercase one character for comparison, keeping it when the mapping is not a
/// single character (the picker compares cell by cell).
fn lowercase(character: char) -> char {
    let mut lowered = character.to_lowercase();
    match (lowered.next(), lowered.next()) {
        (Some(single), None) => single,
        _ => character,
    }
}

#[cfg(test)]
mod tests {
    use super::{fuzzy_match, fuzzy_score};

    #[test]
    fn an_empty_query_matches_everything_without_positions() {
        let matched = fuzzy_match("crates/tui/src/lib.rs", "").unwrap();
        assert_eq!(matched.score(), 0);
        assert!(matched.positions().is_empty());
    }

    #[test]
    fn a_contiguous_run_reports_its_own_cells() {
        let matched = fuzzy_match("crates/tui/src/lib.rs", "tui").unwrap();
        assert_eq!(matched.positions(), [7, 8, 9]);
        assert_eq!(matched.score(), 7);
        assert_eq!(fuzzy_score("crates/tui/src/lib.rs", "tui"), Some(7));
    }

    #[test]
    fn a_gapped_match_reports_each_matched_cell() {
        let matched = fuzzy_match("preview_modal.rs", "pmod").unwrap();
        assert_eq!(matched.positions(), [0, 8, 9, 10]);
        // 連続一致より後ろに並ぶ。
        assert!(matched.score() > fuzzy_match("preview_modal.rs", "modal").unwrap().score());
    }

    #[test]
    fn case_and_width_fold_before_comparing() {
        assert_eq!(
            fuzzy_match("README.md", "readme").unwrap().positions(),
            [0, 1, 2, 3, 4, 5]
        );
        // 1 文字に畳めない大文字はそのまま比較する（İ は 2 文字へ展開される）。
        assert!(fuzzy_match("İstanbul.md", "İst").is_some());
    }

    #[test]
    fn a_query_longer_than_the_candidate_or_out_of_order_does_not_match() {
        assert_eq!(fuzzy_match("a.rs", "abcdefg"), None);
        assert_eq!(fuzzy_score("cleanup", "xyz"), None);
    }
}

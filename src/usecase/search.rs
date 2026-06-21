//! The full-text match rule shared by issue and memory search.
//!
//! Both surfaces fold the query and the candidate fields with Unicode-aware
//! [`str::to_lowercase`] and match on [`str::contains`], so the fold works for
//! the Japanese text the UI carries and a multi-byte needle can never match
//! across a character boundary (an ASCII byte-window match would get both
//! wrong). Keeping the rule here means issue and memory search can never quietly
//! diverge — a change to the matching (e.g. a different normalisation) updates
//! both at once.

/// Case-fold a search query once, so the (potentially many) per-item matches do
/// not each recompute it. Pass the result to [`matches_folded`].
pub fn fold_query(query: &str) -> String {
    query.to_lowercase()
}

/// Whether any `field` contains the already-folded `needle`. An empty needle
/// matches everything (an empty query lists all items).
pub fn matches_folded(needle: &str, fields: &[&str]) -> bool {
    needle.is_empty() || fields.iter().any(|f| f.to_lowercase().contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_matches_everything() {
        let needle = fold_query("");
        assert!(matches_folded(&needle, &["anything"]));
        assert!(matches_folded(&needle, &[]));
    }

    #[test]
    fn matching_is_case_insensitive_across_fields() {
        let needle = fold_query("LOGIN");
        assert!(matches_folded(&needle, &["Fix the login flow", "body"]));
        assert!(matches_folded(&needle, &["title", "the LOGIN screen"]));
        assert!(!matches_folded(&needle, &["unrelated", "text"]));
    }

    #[test]
    fn matching_folds_non_ascii_text() {
        // A Japanese needle folds and matches without splitting a code point.
        let needle = fold_query("ログイン");
        assert!(matches_folded(&needle, &["ログイン画面の修正"]));
    }
}
